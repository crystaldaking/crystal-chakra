resource "null_resource" "provider" {
  triggers = {
    region = var.region
  }
}

module "shared" {
  source = "./modules/shared"
}
