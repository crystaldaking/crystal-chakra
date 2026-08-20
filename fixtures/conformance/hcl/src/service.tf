# CONFORMANCE_TEXT_NEEDLE: payment pipeline marker

resource "null_resource" "dispatch_conformance_request" {
  triggers = {
    target = null_resource.shared_unique_target.id
  }
}
