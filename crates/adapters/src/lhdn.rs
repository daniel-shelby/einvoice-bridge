//! LHDN MyInvois HTTP client. Stub — real implementation in step 4.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LhdnEnv {
    Preprod,
    Prod,
}

impl LhdnEnv {
    pub fn base_url(self) -> &'static str {
        match self {
            LhdnEnv::Preprod => "https://preprod-api.myinvois.hasil.gov.my",
            LhdnEnv::Prod => "https://api.myinvois.hasil.gov.my",
        }
    }
}

pub struct LhdnClient {
    _http: reqwest::Client,
    _env: LhdnEnv,
}

impl LhdnClient {
    pub fn new(env: LhdnEnv) -> Self {
        Self {
            _http: reqwest::Client::new(),
            _env: env,
        }
    }
}
