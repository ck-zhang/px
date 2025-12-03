pub mod commands {
    pub const INIT: &str = "PX101";
    pub const ADD: &str = "PX110";
    pub const REMOVE: &str = "PX111";
    pub const SYNC: &str = "PX120";
    pub const UPDATE: &str = "PX130";
    pub const STATUS: &str = "PX140";
    pub const RUN: &str = "PX201";
    pub const TEST: &str = "PX202";
    pub const FMT: &str = "PX301";
    pub const BUILD: &str = "PX401";
    pub const PUBLISH: &str = "PX402";
    pub const MIGRATE: &str = "PX501";
    pub const WHY: &str = "PX702";
    pub const TOOL: &str = "PX640";
    pub const PYTHON: &str = "PX650";
    pub const GENERIC: &str = "PX000";
}

#[allow(dead_code)]
pub mod cas {
    pub const MISSING_OR_CORRUPT: &str = "PX800";
    pub const STORE_WRITE_FAILURE: &str = "PX810";
    pub const INDEX_CORRUPT: &str = "PX811";
    pub const FORMAT_INCOMPATIBLE: &str = "PX812";
}
