use std::sync::Arc;

use pb::ext::UlidExt;
use pb::scuffle::video::v1::types::access_token_scope::Permission;
use pb::scuffle::video::v1::types::Resource;
use pb::scuffle::video::v1::{RecordingGetRequest, RecordingGetResponse};
use tonic::Status;
use video_common::database::{AccessToken, DatabaseTable};

use crate::api::utils::{get, impl_request_scopes, ApiRequest, TonicRequest};
use crate::global::ApiGlobal;
use crate::ratelimit::RateLimitResource;

impl_request_scopes!(
	RecordingGetRequest,
	video_common::database::Recording,
	(Resource::Recording, Permission::Read),
	RateLimitResource::RecordingGet
);

#[async_trait::async_trait]
impl ApiRequest<RecordingGetResponse> for tonic::Request<RecordingGetRequest> {
	async fn process<G: ApiGlobal>(
		&self,
		global: &Arc<G>,
		access_token: &AccessToken,
	) -> tonic::Result<tonic::Response<RecordingGetResponse>> {
		let req = self.get_ref();

		let mut qb = sqlx::query_builder::QueryBuilder::default();
		qb.push("SELECT * FROM ")
			.push(<RecordingGetRequest as TonicRequest>::Table::NAME)
			.push(" WHERE ");
		let mut seperated = qb.separated(" AND ");

		get::organization_id(&mut seperated, access_token.organization_id);
		get::ids(&mut seperated, &req.ids);

		if let Some(room_id) = req.room_id.as_ref() {
			seperated.push("room_id = ");
			seperated.push_bind_unseparated(room_id.to_uuid());
		}

		if let Some(recording_config_id) = req.recording_config_id.as_ref() {
			seperated.push("recording_config_id = ");
			seperated.push_bind_unseparated(recording_config_id.to_uuid());
		}

		if let Some(s3_bucket_id) = req.s3_bucket_id.as_ref() {
			seperated.push("s3_bucket_id = ");
			seperated.push_bind_unseparated(s3_bucket_id.to_uuid());
		}

		if let Some(public) = req.public {
			seperated.push("public = ");
			seperated.push_bind(public);
		}

		if let Some(deleted) = req.deleted {
			if deleted {
				seperated.push("deleted_at IS NOT NULL");
			} else {
				seperated.push("deleted_at IS NULL");
			}
		}

		get::search_options(&mut seperated, req.search_options.as_ref())?;

		let results = qb.build_query_as::<<RecordingGetRequest as TonicRequest>::Table>().fetch_all(global.db().as_ref()).await.map_err(|err| {
			tracing::error!(err = %err, "failed to fetch {}s", <<RecordingGetRequest as TonicRequest>::Table as DatabaseTable>::FRIENDLY_NAME);
			Status::internal(format!("failed to fetch {}s", <<RecordingGetRequest as TonicRequest>::Table as DatabaseTable>::FRIENDLY_NAME))
		})?;

		let states = global
			.recording_state_loader()
			.load_many(results.iter().map(|recording| recording.id.0))
			.await
			.map_err(|_| Status::internal("failed to load recording states"))?;

		let default_state = Default::default();

		Ok(tonic::Response::new(RecordingGetResponse {
			recordings: results
				.into_iter()
				.map(|recording| {
					states
						.get(&recording.id.0)
						.unwrap_or(&default_state)
						.recording_to_proto(recording)
				})
				.collect(),
		}))
	}
}
