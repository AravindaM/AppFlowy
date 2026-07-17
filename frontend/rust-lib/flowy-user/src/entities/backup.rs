use flowy_derive::ProtoBuf;
use lib_infra::validator_fn::required_not_empty_str;
use validator::Validate;

#[derive(ProtoBuf, Validate, Default)]
pub struct BackupWorkspacePB {
  #[pb(index = 1)]
  #[validate(custom(function = "required_not_empty_str"))]
  pub staging_dir: String,
}
