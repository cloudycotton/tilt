//! H.264: colour conversion, the OpenH264 encoder and SPS inspection.

pub mod convert;
pub mod encoder;
// Only the tests use it: the web client derives the codec string from the SPS itself.
#[cfg(test)]
pub mod sps;
