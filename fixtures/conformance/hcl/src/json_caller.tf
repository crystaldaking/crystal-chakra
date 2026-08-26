# Native-syntax caller of the JSON-declared resource (issue #86).

resource "null_resource" "json_native_caller" {
  triggers = {
    target = null_resource.json_declared_marker.id
  }
}
