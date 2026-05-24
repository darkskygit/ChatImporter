use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use anyhow::Result;
use sqlx::{QueryBuilder, Sqlite};

use super::core::StoredRecord;
use super::{ChatStore, Query, Record, INDEX_NAME};

const SQLITE_PARAM_LIMIT: usize = 999;

impl ChatStore {
    pub async fn query(&self, query: Query) -> Result<Vec<Record>> {
        if let Some(keyword) = query.keyword.as_ref().filter(|keyword| !keyword.is_empty()) {
            self.query_keyword(&query, keyword).await
        } else {
            self.query_sql(&query).await
        }
    }

    pub(super) async fn load_all_records(&self) -> Result<Vec<StoredRecord>> {
        Ok(sqlx::query_as::<_, StoredRecord>(
            r#"
            SELECT id, chat_type, owner_id, group_id, sender_id, sender_name, content, timestamp, metadata
            FROM chat_records
            "#,
        )
        .fetch_all(&self.pool)
        .await?)
    }

    async fn query_sql(&self, query: &Query) -> Result<Vec<Record>> {
        let mut builder = self.filtered_records_builder(query, true);
        builder.push(" ORDER BY timestamp DESC, id DESC");
        builder.push(" LIMIT ");
        builder.push_bind(query.limit() as i64);
        builder.push(" OFFSET ");
        builder.push_bind(query.offset() as i64);
        let rows = builder
            .build_query_as::<StoredRecord>()
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(Record::from).collect())
    }

    async fn query_keyword(&self, query: &Query, keyword: &str) -> Result<Vec<Record>> {
        let hits = self.index.search_hits(INDEX_NAME, keyword);
        if hits.is_empty() {
            return Ok(Vec::new());
        }
        let scores = hits
            .iter()
            .map(|hit| (hit.doc_id.clone(), hit.score))
            .collect::<HashMap<_, _>>();
        let ids = hits
            .iter()
            .filter_map(|hit| hit.doc_id.parse::<i64>().ok())
            .collect::<HashSet<_>>();

        let mut rows = if ids.len() <= SQLITE_PARAM_LIMIT - 16 {
            let mut builder = self.filtered_records_builder(query, false);
            builder.push(" AND id IN (");
            let mut separated = builder.separated(", ");
            for id in &ids {
                separated.push_bind(id);
            }
            separated.push_unseparated(")");
            builder
                .build_query_as::<StoredRecord>()
                .fetch_all(&self.pool)
                .await?
        } else {
            self.filtered_records_builder(query, false)
                .build_query_as::<StoredRecord>()
                .fetch_all(&self.pool)
                .await?
                .into_iter()
                .filter(|record| ids.contains(&record.id))
                .collect()
        };
        rows.sort_by(|left, right| {
            let left_score = scores
                .get(&left.id.to_string())
                .copied()
                .unwrap_or_default();
            let right_score = scores
                .get(&right.id.to_string())
                .copied()
                .unwrap_or_default();
            right_score
                .partial_cmp(&left_score)
                .unwrap_or(Ordering::Equal)
                .then_with(|| right.timestamp.cmp(&left.timestamp))
                .then_with(|| right.id.cmp(&left.id))
        });
        Ok(rows
            .into_iter()
            .skip(query.offset() as usize)
            .take(query.limit() as usize)
            .map(Record::from)
            .collect())
    }

    fn filtered_records_builder<'q>(
        &'q self,
        query: &'q Query,
        include_keyword: bool,
    ) -> QueryBuilder<'q, Sqlite> {
        let mut builder = QueryBuilder::new(
            r#"
            SELECT id, chat_type, owner_id, group_id, sender_id, sender_name, content, timestamp, metadata
            FROM chat_records
            WHERE 1 = 1
            "#,
        );
        if let Some(value) = &query.chat_type {
            builder.push(" AND chat_type = ");
            builder.push_bind(value);
        }
        if let Some(value) = &query.owner_id {
            builder.push(" AND owner_id = ");
            builder.push_bind(value);
        }
        if let Some(value) = &query.group_id {
            builder.push(" AND group_id = ");
            builder.push_bind(value);
        }
        if let Some(value) = &query.sender_id {
            builder.push(" AND sender_id = ");
            builder.push_bind(value);
        }
        if let Some(value) = &query.sender_name {
            builder.push(" AND sender_name = ");
            builder.push_bind(value);
        }
        if let Some(value) = query.before {
            builder.push(" AND timestamp <= ");
            builder.push_bind(value);
        }
        if let Some(value) = query.after {
            builder.push(" AND timestamp >= ");
            builder.push_bind(value);
        }
        if include_keyword {
            if let Some(value) = &query.keyword {
                builder.push(" AND content LIKE ");
                builder.push_bind(format!("%{}%", value));
            }
        }
        builder
    }
}
