use crate::resources::CloudflareDNSRecord;
use chrono::prelude::*;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{
    Condition,
    Time,
};

pub(crate) fn error_condition(
    current: &CloudflareDNSRecord,
    reason: impl ToString,
    message: impl ToString,
    observed_generation: Option<i64>,
) -> Condition {
    let conditions = current.status.as_ref().and_then(|status| status.conditions.as_ref());

    let (was_ready, last_condition) = last_ready_condition(conditions);

    let last_transition_time = if was_ready {
        Time(Utc::now())
    } else {
        last_condition
            .map(|condition| condition.last_transition_time.clone())
            .unwrap_or_else(|| Time(Utc::now()))
    };

    Condition {
        type_: "Ready".to_string(),
        status: "False".to_string(),
        reason: reason.to_string(),
        message: message.to_string(),
        last_transition_time,
        observed_generation,
    }
}

pub(crate) fn success_condition(current: &CloudflareDNSRecord, observed_generation: Option<i64>) -> Condition {
    let conditions = current.status.as_ref().and_then(|status| status.conditions.as_ref());

    let (was_ready, last_condition) = last_ready_condition(conditions);

    let last_transition_time = if !was_ready {
        Time(Utc::now())
    } else {
        last_condition
            .map(|condition| condition.last_transition_time.clone())
            .unwrap_or_else(|| Time(Utc::now()))
    };

    Condition {
        type_: "Ready".to_string(),
        status: "True".to_string(),
        reason: "Sucessfully applied changes".to_string(),
        message: "DNS record ready".to_string(),
        last_transition_time,
        observed_generation,
    }
}

fn last_ready_condition(conditions: Option<&Vec<Condition>>) -> (bool, Option<&Condition>) {
    let (was_ready, last_condition) = conditions.map_or((true, None), |conditions| {
        let ready_cond = conditions.iter().find(|condition| condition.type_ == "Ready");
        (
            ready_cond.map_or(false, |condition| condition.status == "True"),
            ready_cond,
        )
    });
    (was_ready, last_condition)
}
