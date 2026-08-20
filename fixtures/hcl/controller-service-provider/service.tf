resource "null_resource" "service" {
  triggers = {
    provider_id = null_resource.provider.id
    name        = local.service_name
  }
}
