use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::sync::Arc;

use crate::auth::Authenticated;
use crate::database::{Cond, Database, Program, Step};
use crate::error::Error as SqldError;
use crate::hrana::stmt::StmtError;
use crate::query::{Params, Query};
use crate::query_analysis::Statement;
use crate::query_result_builder::{QueryResultBuilder, StepResult, StepResultsBuilder};

use super::result_builder::HranaBatchProtoBuilder;
use super::stmt::{proto_stmt_to_query, stmt_error_from_sqld_error};
use super::{proto, ProtocolError, Version};

#[derive(thiserror::Error, Debug)]
pub enum BatchError {
    #[error("Transaction timed out")]
    TransactionTimeout,
    #[error("Server cannot handle additional transactions")]
    TransactionBusy,
}

fn proto_cond_to_cond(cond: &proto::BatchCond, max_step_i: usize) -> Result<Cond> {
    let try_convert_step = |step: i32| -> Result<usize, ProtocolError> {
        let step = usize::try_from(step).map_err(|_| ProtocolError::BatchCondBadStep)?;
        if step >= max_step_i {
            return Err(ProtocolError::BatchCondBadStep);
        }
        Ok(step)
    };

    let cond = match cond {
        proto::BatchCond::Ok { step } => Cond::Ok {
            step: try_convert_step(*step)?,
        },
        proto::BatchCond::Error { step } => Cond::Err {
            step: try_convert_step(*step)?,
        },
        proto::BatchCond::Not { cond } => Cond::Not {
            cond: proto_cond_to_cond(cond, max_step_i)?.into(),
        },
        proto::BatchCond::And { conds } => Cond::And {
            conds: conds
                .iter()
                .map(|cond| proto_cond_to_cond(cond, max_step_i))
                .collect::<Result<_>>()?,
        },
        proto::BatchCond::Or { conds } => Cond::Or {
            conds: conds
                .iter()
                .map(|cond| proto_cond_to_cond(cond, max_step_i))
                .collect::<Result<_>>()?,
        },
    };

    Ok(cond)
}

pub fn proto_batch_to_program(
    batch: &proto::Batch,
    sqls: &HashMap<i32, String>,
    version: Version,
) -> Result<Program> {
    let mut steps = Vec::with_capacity(batch.steps.len());
    for (step_i, step) in batch.steps.iter().enumerate() {
        let query = proto_stmt_to_query(&step.stmt, sqls, version)?;
        let cond = step
            .condition
            .as_ref()
            .map(|cond| proto_cond_to_cond(cond, step_i))
            .transpose()?;
        let step = Step { query, cond };

        steps.push(step);
    }

    Ok(Program::new(steps))
}

pub async fn execute_batch(
    db: &impl Database,
    auth: Authenticated,
    pgm: Program,
) -> Result<proto::BatchResult> {
    let batch_builder = HranaBatchProtoBuilder::default();
    let (builder, _state) = db
        .execute_program(pgm, auth, batch_builder)
        .await
        .map_err(catch_batch_error)?;

    Ok(builder.into_ret())
}

pub fn proto_sequence_to_program(sql: &str) -> Result<Program> {
    let stmts = Statement::parse(sql)
        .collect::<Result<Vec<_>>>()
        .map_err(|err| anyhow!(StmtError::SqlParse { source: err }))?;

    let steps = stmts
        .into_iter()
        .enumerate()
        .map(|(step_i, stmt)| {
            let cond = match step_i {
                0 => None,
                _ => Some(Cond::Ok { step: step_i - 1 }),
            };
            let query = Query {
                stmt,
                params: Params::empty(),
                want_rows: false,
            };
            Step { cond, query }
        })
        .collect();
    Ok(Program {
        steps: Arc::new(steps),
    })
}

pub async fn execute_sequence(db: &impl Database, auth: Authenticated, pgm: Program) -> Result<()> {
    let builder = StepResultsBuilder::default();
    let (builder, _state) = db
        .execute_program(pgm, auth, builder)
        .await
        .map_err(catch_batch_error)?;
    builder
        .into_ret()
        .into_iter()
        .try_for_each(|result| match result {
            StepResult::Ok => Ok(()),
            StepResult::Err(e) => match stmt_error_from_sqld_error(e) {
                Ok(stmt_err) => Err(anyhow!(stmt_err)),
                Err(sqld_err) => Err(anyhow!(sqld_err)),
            },
            StepResult::Skipped => Err(anyhow!("Statement in sequence was not executed")),
        })
}

fn catch_batch_error(sqld_error: SqldError) -> anyhow::Error {
    match batch_error_from_sqld_error(sqld_error) {
        Ok(stmt_error) => anyhow!(stmt_error),
        Err(sqld_error) => anyhow!(sqld_error),
    }
}

fn batch_error_from_sqld_error(sqld_error: SqldError) -> Result<BatchError, SqldError> {
    Ok(match sqld_error {
        SqldError::LibSqlTxTimeout => BatchError::TransactionTimeout,
        SqldError::LibSqlTxBusy => BatchError::TransactionBusy,
        sqld_error => return Err(sqld_error),
    })
}

impl BatchError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::TransactionTimeout => "TRANSACTION_TIMEOUT",
            Self::TransactionBusy => "TRANSACTION_BUSY",
        }
    }
}
