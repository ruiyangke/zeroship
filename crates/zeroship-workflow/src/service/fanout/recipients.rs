use super::{models, AppId, Broadcast, Transaction, WorkflowServiceError};

pub(super) async fn select(
    tx: &Transaction,
    app: &AppId,
    broadcast: &Broadcast,
    limit: u32,
) -> Result<Vec<models::SubscriptionRecipient>, WorkflowServiceError> {
    let db = tx.database();
    let subscription = db.entity::<models::subscriptions::Entity>()?.alias("s")?;
    let run = db.entity::<models::runs::Entity>()?.alias("r")?;
    let wait = db.entity::<models::waits::Entity>()?.alias("w")?;
    let recipients = db
        .from(&subscription)
        .inner_join(
            &run,
            subscription
                .column(models::subscriptions::app_id)
                .eq(run.column(models::runs::app_id))?
                .and(
                    subscription
                        .column(models::subscriptions::run_id)
                        .eq(run.column(models::runs::id))?,
                )
                .and(
                    subscription
                        .column(models::subscriptions::generation)
                        .eq(run.column(models::runs::generation))?,
                ),
        )?
        .inner_join(
            &wait,
            subscription
                .column(models::subscriptions::app_id)
                .eq(wait.column(models::waits::app_id))?
                .and(
                    subscription
                        .column(models::subscriptions::run_id)
                        .eq(wait.column(models::waits::run_id))?,
                )
                .and(
                    subscription
                        .column(models::subscriptions::generation)
                        .eq(wait.column(models::waits::generation))?,
                )
                .and(
                    subscription
                        .column(models::subscriptions::ordinal)
                        .eq(wait.column(models::waits::ordinal))?,
                ),
        )?
        .filter(
            subscription
                .column(models::subscriptions::app_id)
                .eq(app.as_str())?
                .and(
                    subscription
                        .column(models::subscriptions::topic)
                        .eq(broadcast.topic.as_str())?,
                )
                .and(
                    subscription
                        .column(models::subscriptions::sequence)
                        .gt(broadcast.cursor)?,
                )
                .and(
                    subscription
                        .column(models::subscriptions::sequence)
                        .lte(broadcast.cutoff_sequence)?,
                )
                .and(
                    wait.column(models::waits::signal_type)
                        .eq(Some(broadcast.signal_type.as_str()))?,
                )
                .and(
                    run.column(models::runs::state)
                        .in_values(["completed", "failed", "cancelled"])?
                        .negate(),
                ),
        )
        .order_by(subscription.column(models::subscriptions::sequence).asc())
        .select(subscription.row::<models::SubscriptionRecipient>())?
        .limit(i64::from(limit))?
        .all()
        .await?;
    Ok(recipients)
}
