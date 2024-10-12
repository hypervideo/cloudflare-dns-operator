pub struct ControllerState {
    pub client: kube::Client,
    pub cloudflare_api_token: String,
}
