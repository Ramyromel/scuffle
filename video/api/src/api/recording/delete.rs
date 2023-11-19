use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures_util::StreamExt;
use pb::ext::UlidExt;
use pb::scuffle::video::internal::events::{recording_delete_batch_task, RecordingDeleteBatchTask};
use pb::scuffle::video::v1::types::access_token_scope::Permission;
use pb::scuffle::video::v1::types::{FailedResource, Resource};
use pb::scuffle::video::v1::{RecordingDeleteRequest, RecordingDeleteResponse};
use prost::Message;
use ulid::Ulid;
use video_common::database::{AccessToken, DatabaseTable, Rendition};

use crate::api::utils::{impl_request_scopes, ApiRequest, TonicRequest};
use crate::config::ApiConfig;
use crate::global::ApiGlobal;
use crate::ratelimit::RateLimitResource;

impl_request_scopes!(
	RecordingDeleteRequest,
	video_common::database::Recording,
	(Resource::Recording, Permission::Delete),
	RateLimitResource::RecordingDelete
);

#[derive(sqlx::FromRow)]
struct ThumbnailResp {
	recording_id: common::database::Ulid,
	thumbnail_id: common::database::Ulid,
	idx: i32,
}

#[derive(sqlx::FromRow)]
struct SegmentResp {
	recording_id: common::database::Ulid,
	segment_id: common::database::Ulid,
	idx: i32,
	rendition: Rendition,
}

trait UpdateBatch {
	fn is_same_batch(&self, batch: &RecordingDeleteBatchTask) -> bool;
	fn update_batch(&self, deleted_recordings: &HashMap<Ulid, Ulid>, batch: &mut RecordingDeleteBatchTask);
	fn to_object(&self) -> recording_delete_batch_task::Object;
}

impl UpdateBatch for ThumbnailResp {
	fn is_same_batch(&self, batch: &RecordingDeleteBatchTask) -> bool {
		batch.recording_id.to_ulid() == self.recording_id.0
			&& matches!(batch.object_types, Some(recording_delete_batch_task::ObjectTypes::Thumbnails(_)))
	}

	fn update_batch(&self, deleted_recordings: &HashMap<Ulid, Ulid>, batch: &mut RecordingDeleteBatchTask) {
		batch.recording_id = Some(self.recording_id.0.into());
		batch.s3_bucket_id = Some(deleted_recordings[&self.recording_id.0].into());
		batch.object_types = Some(recording_delete_batch_task::ObjectTypes::Thumbnails(
			recording_delete_batch_task::ThumbnailType {},
		));
		batch.objects.clear();
	}

	fn to_object(&self) -> recording_delete_batch_task::Object {
		recording_delete_batch_task::Object {
			index: self.idx,
			object_id: Some(self.thumbnail_id.0.into()),
		}
	}
}

impl UpdateBatch for SegmentResp {
	fn is_same_batch(&self, batch: &RecordingDeleteBatchTask) -> bool {
		batch.recording_id.to_ulid() == self.recording_id.0
			&& batch.object_types
				== Some(recording_delete_batch_task::ObjectTypes::Segments(
					pb::scuffle::video::v1::types::Rendition::from(self.rendition) as i32,
				))
	}

	fn update_batch(&self, deleted_recordings: &HashMap<Ulid, Ulid>, batch: &mut RecordingDeleteBatchTask) {
		batch.recording_id = Some(self.recording_id.0.into());
		batch.s3_bucket_id = Some(deleted_recordings[&self.recording_id.0].into());
		batch.object_types = Some(recording_delete_batch_task::ObjectTypes::Segments(
			pb::scuffle::video::v1::types::Rendition::from(self.rendition) as i32,
		));
		batch.objects.clear();
	}

	fn to_object(&self) -> recording_delete_batch_task::Object {
		recording_delete_batch_task::Object {
			index: self.idx,
			object_id: Some(self.segment_id.0.into()),
		}
	}
}

async fn handle_resp(
	global: &Arc<impl ApiGlobal>,
	deleted_recordings: &HashMap<Ulid, Ulid>,
	resp: impl UpdateBatch,
	batch: &mut RecordingDeleteBatchTask,
) -> Result<(), ()> {
	if resp.is_same_batch(batch) {
		batch.objects.push(resp.to_object());
	} else {
		publish_batch(global, batch).await?;
		resp.update_batch(deleted_recordings, batch);
		batch.objects.push(resp.to_object());
	}

	Ok(())
}

async fn publish_batch(global: &Arc<impl ApiGlobal>, batch: &RecordingDeleteBatchTask) -> Result<(), ()> {
	if !batch.objects.is_empty() {
		global
			.nats()
			.publish(
				global.config::<ApiConfig>().recording_delete_stream.clone(),
				batch.encode_to_vec().into(),
			)
			.await
			.map_err(|err| {
				tracing::error!(err = %err, "failed to publish recording delete batch");
			})?;
	}

	Ok(())
}

async fn handle_end_of_stream(global: &Arc<impl ApiGlobal>, batch: &mut RecordingDeleteBatchTask) -> Result<(), ()> {
	publish_batch(global, batch).await?;

	// Reset the batch
	batch.recording_id = None;
	batch.s3_bucket_id = None;
	batch.object_types = None;
	batch.objects.clear();

	Ok(())
}

#[async_trait::async_trait]
impl ApiRequest<RecordingDeleteResponse> for tonic::Request<RecordingDeleteRequest> {
	async fn process<G: ApiGlobal>(
		&self,
		global: &Arc<G>,
		access_token: &AccessToken,
	) -> tonic::Result<tonic::Response<RecordingDeleteResponse>> {
		let mut qb = sqlx::query_builder::QueryBuilder::default();

		let req = self.get_ref();

		if req.ids.len() > 100 {
			return Err(tonic::Status::invalid_argument("too many ids provided for delete: max 100".to_string()));
		}

		if req.ids.is_empty() {
			return Err(tonic::Status::invalid_argument("no ids provided for delete"));
		}

		let mut ids_to_delete = req.ids.iter().map(pb::ext::UlidExt::to_ulid).collect::<HashSet<_>>();

		let mut tx = global.db().begin().await.map_err(|err| {
			tracing::error!(err = %err, "failed to begin transaction");
			tonic::Status::internal("failed to begin transaction, the recording may have been deleted")
		})?;

		// We dont actually want to delete the recordings from the database, we just
		// want to mark them as deleted
		qb.push("UPDATE ")
			.push(<RecordingDeleteRequest as TonicRequest>::Table::NAME)
			.push(" SET deleted_at = NOW(), room_id = NULL, recording_config_id = NULL")
			.push(" WHERE id = ANY(")
			.push_bind(ids_to_delete.iter().copied().map(common::database::Ulid).collect::<Vec<_>>())
			.push(") AND organization_id = ")
			.push_bind(access_token.organization_id)
			.push(" AND deleted_at IS NULL")
			.push(" RETURNING id, s3_bucket_id");

		let deleted_recordings: Vec<(common::database::Ulid, common::database::Ulid)> =
			qb.build_query_scalar().fetch_all(tx.as_mut()).await.map_err(|err| {
				tracing::error!(err = %err, "failed to update {}s", <RecordingDeleteRequest as TonicRequest>::Table::FRIENDLY_NAME);
				tonic::Status::internal(format!(
					"failed to delete {}s",
					<RecordingDeleteRequest as TonicRequest>::Table::FRIENDLY_NAME
				))
			})?;

		let deleted_ids = deleted_recordings.iter().map(|(id, _)| *id).collect::<Vec<_>>();

		let deleted_recordings = deleted_recordings
			.into_iter()
			.map(|(id, bucket_id)| (id.0, bucket_id.0))
			.collect::<HashMap<_, _>>();

		deleted_ids.iter().for_each(|id| {
			ids_to_delete.remove(&id.0);
		});

		let mut qb = sqlx::query_builder::QueryBuilder::default();

		qb.push("DELETE FROM ")
			.push(<video_common::database::PlaybackSession as DatabaseTable>::NAME)
			.push(" WHERE recording_id = ANY(")
			.push_bind(&deleted_ids)
			.push(") AND organization_id = ")
			.push_bind(access_token.organization_id);

		qb.build().execute(tx.as_mut()).await.map_err(|err| {
			tracing::error!(err = %err, "failed to delete {}s", <video_common::database::PlaybackSession as DatabaseTable>::FRIENDLY_NAME);
			tonic::Status::internal(format!("failed to delete {}s, the recording have not been deleted", <video_common::database::PlaybackSession as DatabaseTable>::FRIENDLY_NAME))
		})?;

		let mut qb = sqlx::query_builder::QueryBuilder::default();

		qb.push("DELETE FROM ")
			.push(<video_common::database::RecordingRendition as DatabaseTable>::NAME)
			.push(" WHERE recording_id = ANY(")
			.push_bind(&deleted_ids)
			.push(")");

		qb.build().execute(tx.as_mut()).await.map_err(|err| {
			tracing::error!(err = %err, "failed to delete {}s", <video_common::database::PlaybackSession as DatabaseTable>::FRIENDLY_NAME);
			tonic::Status::internal(format!("failed to delete {}s, the recording have not been deleted", <video_common::database::PlaybackSession as DatabaseTable>::FRIENDLY_NAME))
		})?;

		tx.commit().await.map_err(|err| {
			tracing::error!(err = %err, "failed to commit transaction");
			tonic::Status::internal("failed to commit transaction, the recording have not been deleted")
		})?;

		// The next part is resource cleanup in S3. Regardless if this next part fails
		// we can detect these failures At the database state level and retry the
		// cleanup later.

		let allowed_to_fail = || async {
			let mut batch = RecordingDeleteBatchTask {
				recording_id: None,
				s3_bucket_id: None,
				object_types: None,
				objects: Vec::with_capacity(global.config::<ApiConfig>().recording_delete_batch_size),
			};

			let mut qb = sqlx::query_builder::QueryBuilder::default();

			qb.push("SELECT id, recording_id, idx, FROM ")
				.push(<video_common::database::RecordingThumbnail as DatabaseTable>::NAME)
				.push(" WHERE recording_id = ANY(")
				.push_bind(&deleted_ids)
				.push(") ORDER BY recording_id");

			while let Some(result) = qb
				.build_query_as::<ThumbnailResp>()
				.fetch_many(global.db().as_ref())
				.next()
				.await
			{
				let result = result.map_err(|err| {
					tracing::error!(err = %err, "failed to select recording thumbnails");
				})?;

				let Some(result) = result.right() else {
					break;
				};

				handle_resp(global, &deleted_recordings, result, &mut batch).await?;
			}

			handle_end_of_stream(global, &mut batch).await?;

			let mut qb = sqlx::query_builder::QueryBuilder::default();

			qb.push("SELECT id, recording_id, rendition, idx FROM ")
				.push(<video_common::database::RecordingRenditionSegment as DatabaseTable>::NAME)
				.push(" WHERE recording_id = ANY(")
				.push_bind(&deleted_ids)
				.push(") ORDER BY recording_id, rendition");

			while let Some(result) = qb
				.build_query_as::<SegmentResp>()
				.fetch_many(global.db().as_ref())
				.next()
				.await
			{
				let result = result.map_err(|err| {
					tracing::error!(err = %err, "failed to delete recording segments");
				})?;

				let Some(result) = result.right() else {
					break;
				};

				handle_resp(global, &deleted_recordings, result, &mut batch).await?;
			}

			handle_end_of_stream(global, &mut batch).await?;

			Result::<(), ()>::Ok(())
		};

		// We don't care about this error because even if the publish fails, we can
		// detect the failure at the database state level and retry the cleanup later.
		let _ = allowed_to_fail().await;

		Ok(tonic::Response::new(RecordingDeleteResponse {
			ids: deleted_ids.into_iter().map(|id| id.0.into()).collect(),
			failed_deletes: ids_to_delete
				.into_iter()
				.map(|id| FailedResource {
					id: Some(id.into()),
					reason: "recording not found".into(),
				})
				.collect(),
		}))
	}
}
