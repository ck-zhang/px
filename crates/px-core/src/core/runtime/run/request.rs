#[derive(Clone, Debug)]
pub struct TestRequest {
    pub args: Vec<String>,
    pub frozen: bool,
    pub sandbox: bool,
    pub at: Option<String>,
}

#[derive(Clone, Debug)]
pub struct RunRequest {
    pub entry: Option<String>,
    pub target: Option<String>,
    pub args: Vec<String>,
    pub frozen: bool,
    pub allow_floating: bool,
    pub interactive: Option<bool>,
    pub sandbox: bool,
    pub at: Option<String>,
}
