use pb::scuffle::video::v1::types::access_token_scope::Permission;
use pb::scuffle::video::v1::types::Resource;
use pb::scuffle::video::v1::{RecordingTagRequest, RecordingTagResponse};

use crate::api::utils::impl_request_scopes;
use crate::api::utils::tags::impl_tag_req;
use crate::ratelimit::RateLimitResource;

impl_request_scopes!(
	RecordingTagRequest,
	video_common::database::Recording,
	(Resource::Recording, Permission::Modify),
	RateLimitResource::RecordingTag
);

impl_tag_req!(RecordingTagRequest, RecordingTagResponse);
