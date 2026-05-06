// generator-go-server emits a minimal `net/http` server scaffold from a
// forge IR — the reference cross-language plugin (issue #58).
//
// Reads operation id, method, path-template, and path-param types. Emits
// `go.mod` and `server.go` containing a `Server` interface (one method per
// operation) and `RegisterRoutes(*http.ServeMux, Server)` that decodes path
// values and dispatches.
//
// Out of scope: typed request/response bodies, query/header param decoding,
// authentication. Implementations handle those off `*http.Request` directly.
package main

import (
	"encoding/json"
	"fmt"

	generatorapi "github.com/MarcusDunn/openapi-forge/plugins/generator-go-server/internal/bindings/forge/plugin/generator-api"
	"github.com/MarcusDunn/openapi-forge/plugins/generator-go-server/internal/bindings/forge/plugin/stage"
	"github.com/MarcusDunn/openapi-forge/plugins/generator-go-server/internal/bindings/forge/plugin/types"
	"github.com/MarcusDunn/openapi-forge/plugins/generator-go-server/emit"

	"go.bytecodealliance.org/cm"
)

const (
	pluginName    = "generator-go-server"
	pluginVersion = "0.1.0"

	configSchema = `{
  "type": "object",
  "additionalProperties": false,
  "required": ["module_path"],
  "properties": {
    "module_path": {
      "type": "string",
      "minLength": 1,
      "description": "Go module path used in go.mod (e.g. \"github.com/example/petstore\")."
    },
    "package_name": {
      "type": "string",
      "minLength": 1,
      "pattern": "^[a-z][a-z0-9]*$",
      "description": "Override the default package name (last segment of module_path)."
    }
  }
}`
)

type config struct {
	ModulePath  string `json:"module_path"`
	PackageName string `json:"package_name"`
}

// main is required for TinyGo's wasip2 target but a no-op — the component
// runtime drives execution through the exported functions, not main.
func main() {}

func init() {
	generatorapi.Exports.Info = info
	generatorapi.Exports.ConfigSchema = func() string { return configSchema }
	generatorapi.Exports.Generate = generate
}

func info() generatorapi.PluginInfo {
	return types.PluginInfo{
		Name:    pluginName,
		Version: pluginVersion,
	}
}

func generate(spec generatorapi.Ir, configJSON string) cm.Result[generatorapi.StageErrorShape, generatorapi.GenerationOutput, generatorapi.StageError] {
	type result = cm.Result[generatorapi.StageErrorShape, generatorapi.GenerationOutput, generatorapi.StageError]

	var cfg config
	if err := json.Unmarshal([]byte(configJSON), &cfg); err != nil {
		return cm.Err[result](stage.StageErrorConfigInvalid(fmt.Sprintf("config json: %v", err)))
	}
	if cfg.ModulePath == "" {
		return cm.Err[result](stage.StageErrorConfigInvalid("config: module_path is required"))
	}
	pkg := cfg.PackageName
	if pkg == "" {
		pkg = emit.PackageName(cfg.ModulePath)
	}

	rendered := emit.Server(pkg, irToSpec(spec))
	gomod := emit.GoMod(cfg.ModulePath)

	files := []generatorapi.OutputFile{
		fileText("go.mod", gomod),
		fileText(pkg+"/server.go", rendered),
	}
	out := generatorapi.GenerationOutput{
		Files: cm.ToList(files),
		// Diagnostics: zero-value empty list.
	}
	return cm.OK[result](out)
}

func fileText(path, content string) generatorapi.OutputFile {
	return generatorapi.OutputFile{
		Path:    path,
		Content: cm.ToList([]uint8(content)),
		Mode:    generatorapi.FileModeText,
	}
}

func irToSpec(spec generatorapi.Ir) emit.Spec {
	out := emit.Spec{
		Title:   spec.Info.Title,
		Version: spec.Info.Version,
	}
	if d := spec.Info.Description.Some(); d != nil {
		out.Description = *d
	}

	typeKinds := indexPrimitiveKinds(spec.Types.Slice())

	ops := spec.Operations.Slice()
	out.Operations = make([]emit.Operation, 0, len(ops))
	for i := range ops {
		op := &ops[i]
		out.Operations = append(out.Operations, emit.Operation{
			ID:         emit.PascalCase(op.ID),
			Method:     methodToString(op.Method),
			Path:       op.PathTemplate,
			PathParams: convertPathParams(op.PathParams.Slice(), typeKinds),
			Doc:        optString(op.Documentation),
		})
	}
	return out
}

// indexPrimitiveKinds maps type-id → primitive Go type. Non-primitives are
// omitted; consumers of the map fall back to "string" for unknowns.
func indexPrimitiveKinds(types []types.NamedType) map[string]string {
	out := make(map[string]string, len(types))
	for i := range types {
		nt := &types[i]
		def := &nt.Definition
		prim := def.Primitive()
		if prim == nil {
			continue
		}
		out[nt.ID] = primitiveGo(prim)
	}
	return out
}

func convertPathParams(in []types.Parameter, kinds map[string]string) []emit.Parameter {
	if len(in) == 0 {
		return nil
	}
	out := make([]emit.Parameter, 0, len(in))
	for i := range in {
		p := &in[i]
		goType, ok := kinds[string(p.Type)]
		if !ok {
			goType = "string"
		}
		out = append(out, emit.Parameter{
			Name:   emit.LowerCamel(p.Name),
			GoType: goType,
		})
	}
	return out
}

func methodToString(m types.HTTPMethod) string {
	// HTTPMethod is rendered by wit-bindgen-go as a payloaded variant
	// (`other(string)` for OAS 3.2), so case constants don't exist —
	// dispatch via the generated predicate methods instead.
	switch {
	case m.Get():
		return "GET"
	case m.Put():
		return "PUT"
	case m.Post():
		return "POST"
	case m.Delete():
		return "DELETE"
	case m.Options():
		return "OPTIONS"
	case m.Head():
		return "HEAD"
	case m.Patch():
		return "PATCH"
	case m.Trace():
		return "TRACE"
	default:
		return "GET"
	}
}

// Per #105 the IR carries the JSON Schema `type` value only; format
// refinements (`int32`, `int64`, `float`, `byte`, `date`, etc.) live on
// `format-extension`. Pick richer Go types when the format hints at a
// width or a binary encoding; everything else collapses to a sensible
// default.
func primitiveGo(p *types.PrimitiveType) string {
	fmt := optString(p.Constraints.FormatExtension)
	switch p.Kind {
	case types.PrimitiveKindPrimString:
		if fmt == "byte" || fmt == "binary" {
			return "[]byte"
		}
		return "string"
	case types.PrimitiveKindPrimInteger:
		if fmt == "int32" {
			return "int32"
		}
		return "int64"
	case types.PrimitiveKindPrimNumber:
		if fmt == "float" {
			return "float32"
		}
		return "float64"
	case types.PrimitiveKindPrimBool:
		return "bool"
	default:
		return "string"
	}
}

func optString(o cm.Option[string]) string {
	if v := o.Some(); v != nil {
		return *v
	}
	return ""
}
