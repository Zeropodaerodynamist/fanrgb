"""A small JSON Schema (2020-12 subset) validator, enough for catalog/schema.json, so CI can validate
the catalog and the manual entries against the documented schema with the standard library only.

Supported: type, const, enum, pattern, minLength, maxLength, minimum, minItems, maxItems, items,
properties, required, $ref (local #/...), anyOf, oneOf. Unknown keywords are ignored, as the spec allows
for annotations ("description", "$id", ...).
"""

import re

_TYPES = {
    "object": lambda v: isinstance(v, dict),
    "array": lambda v: isinstance(v, list),
    "string": lambda v: isinstance(v, str),
    "integer": lambda v: isinstance(v, int) and not isinstance(v, bool),
    "number": lambda v: isinstance(v, (int, float)) and not isinstance(v, bool),
    "boolean": lambda v: isinstance(v, bool),
    "null": lambda v: v is None,
}


class SchemaError(Exception):
    pass


def _ecma(pattern):
    """In ECMA-262 (JSON Schema) a final '$' matches only at the very end; Python's also matches before
    a trailing newline. '\\Z' is the ECMA meaning."""
    if pattern.endswith("$") and not pattern.endswith("\\$"):
        return pattern[:-1] + r"\Z"
    return pattern


def _resolve(root, ref):
    if not ref.startswith("#/"):
        raise SchemaError(f"unsupported $ref {ref}")
    node = root
    for part in ref[2:].split("/"):
        node = node[part]
    return node


def errors(instance, schema, root=None, path="$"):
    """All validation errors as 'path: message' strings (empty list = valid)."""
    root = root if root is not None else schema
    out = []
    if "$ref" in schema:
        out += errors(instance, _resolve(root, schema["$ref"]), root, path)
    t = schema.get("type")
    if t is not None:
        types = t if isinstance(t, list) else [t]
        if not any(_TYPES[x](instance) for x in types):
            return out + [f"{path}: expected {'/'.join(types)}"]
    if "const" in schema and instance != schema["const"]:
        out.append(f"{path}: must be {schema['const']!r}")
    if "enum" in schema and instance not in schema["enum"]:
        out.append(f"{path}: must be one of {schema['enum']}")
    if isinstance(instance, str):
        if "minLength" in schema and len(instance) < schema["minLength"]:
            out.append(f"{path}: shorter than {schema['minLength']}")
        if "maxLength" in schema and len(instance) > schema["maxLength"]:
            out.append(f"{path}: longer than {schema['maxLength']}")
        if "pattern" in schema and not re.search(_ecma(schema["pattern"]), instance):
            out.append(f"{path}: does not match {schema['pattern']}")
    if isinstance(instance, (int, float)) and not isinstance(instance, bool):
        if "minimum" in schema and instance < schema["minimum"]:
            out.append(f"{path}: below {schema['minimum']}")
    if isinstance(instance, list):
        if "minItems" in schema and len(instance) < schema["minItems"]:
            out.append(f"{path}: fewer than {schema['minItems']} items")
        if "maxItems" in schema and len(instance) > schema["maxItems"]:
            out.append(f"{path}: more than {schema['maxItems']} items")
        if "items" in schema:
            for i, v in enumerate(instance):
                out += errors(v, schema["items"], root, f"{path}[{i}]")
    if isinstance(instance, dict):
        for k in schema.get("required", []):
            if k not in instance:
                out.append(f"{path}: missing {k}")
        for k, sub in schema.get("properties", {}).items():
            if k in instance:
                out += errors(instance[k], sub, root, f"{path}.{k}")
    if "anyOf" in schema and not any(not errors(instance, s, root, path) for s in schema["anyOf"]):
        out.append(f"{path}: matches none of the allowed forms")
    if "oneOf" in schema and sum(1 for s in schema["oneOf"] if not errors(instance, s, root, path)) != 1:
        out.append(f"{path}: must match exactly one allowed form")
    return out
