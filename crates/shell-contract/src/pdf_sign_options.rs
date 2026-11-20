//! Request-owned PDF signing options shared by the IPC contract and native codec.

/// The signature options `parseSignOptions` let through.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SignOptions {
    pub reason: Option<String>,
    pub location: Option<String>,
    pub contact_info: Option<String>,
    pub certify_level: Option<u8>,
    pub visible: Option<VisibleSignature>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VisibleSignature {
    pub page_index: f64,
    /// One of `SIGNATURE_POSITIONS`.
    pub position: String,
    pub width: f64,
    pub height: f64,
    pub png: Vec<u8>,
}
