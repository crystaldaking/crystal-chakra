run "test_refund_flow" {
  command = plan

  assert {
    condition     = null_resource.service.id != ""
    error_message = "service must be planned"
  }
}
