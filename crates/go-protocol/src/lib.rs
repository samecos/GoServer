// Prost controls these wire-layout enums; bounded transport queues account for
// the complete message size. Keep the generated public representation intact.
#[allow(clippy::large_enum_variant)]
pub mod v1 {
    tonic::include_proto!("goeval.v1");
}
pub const PROTOCOL_VERSION: u32 = 1;
pub const INPUT_PROFILE: &str = "katago-eval-v1";
