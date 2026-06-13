use serde::{Deserialize, Serialize};

use crate::{view::ThingStatus, ThingDatatype};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BindingRow {
    pub entity_kind: String, // "collection" | "thing"
    pub entity_id: String,
    pub trigger_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CollectionRow {
    pub id: String,
    pub title: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ThingRow {
    pub id: String,
    pub collection_id: String,
    pub datatype: ThingDatatype,
    pub status: ThingStatus,
    pub title: Option<String>,
    pub parent_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct MaterializePlan {
    pub upsert_collections: Vec<CollectionRow>,
    pub delete_collections: Vec<String>,
    pub upsert_things: Vec<ThingRow>,
    pub delete_things: Vec<String>,
    pub set_bindings: Vec<BindingRow>,
    pub clear_bindings: Vec<(String, String)>, // (entity_kind, entity_id)
}

pub fn materialize_plan(view: &View) -> MaterializePlan {
    let mut plan = MaterializePlan::default();

    for c in &view.collections {
        if c.tombstone.as_ref().map(|t| t.deleted).unwrap_or(false) {
            plan.delete_collections.push(c.id.clone());
        } else {
            plan.upsert_collections.push(CollectionRow {
                id: c.id.clone(),
                title: c.title.clone(),
                status: c.status.clone(),
            });
        }

        match c
            .trigger
            .as_ref()
            .and_then(|t| t.uuid.clone())
            .filter(|_| c.tombstone.as_ref().map(|t| t.deleted).unwrap_or(false) == false)
        {
            Some(trigger_id) => plan.set_bindings.push(BindingRow {
                entity_kind: "collection".to_string(),
                entity_id: c.id.clone(),
                trigger_id,
            }),
            None => plan
                .clear_bindings
                .push(("collection".to_string(), c.id.clone())),
        }
    }

    for t in &view.things {
        if t.tombstone.as_ref().map(|x| x.deleted).unwrap_or(false) {
            plan.delete_things.push(t.id.clone());
        } else {
            plan.upsert_things.push(ThingRow {
                id: t.id.clone(),
                collection_id: t.collection_id.clone(),
                datatype: t.datatype.clone(),
                status: t.status.clone(),
                title: t.title.clone(),
                parent_id: t.parent_id.clone(),
            });
        }

        match t
            .trigger
            .as_ref()
            .and_then(|x| x.uuid.clone())
            .filter(|_| t.tombstone.as_ref().map(|t| t.deleted).unwrap_or(false) == false)
        {
            Some(trigger_id) => plan.set_bindings.push(BindingRow {
                entity_kind: "thing".to_string(),
                entity_id: t.id.clone(),
                trigger_id,
            }),
            None => plan
                .clear_bindings
                .push(("thing".to_string(), t.id.clone())),
        }
    }

    plan
}

pub type View = crate::view::View;
