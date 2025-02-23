// Copyright 2023 Greptime Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Python script engine
use std::any::Any;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use async_trait::async_trait;
use common_error::prelude::BoxedError;
use common_function::scalars::{Function, FUNCTION_REGISTRY};
use common_query::error::{PyUdfSnafu, UdfTempRecordBatchSnafu};
use common_query::prelude::Signature;
use common_query::Output;
use common_recordbatch::error::{ExternalSnafu, Result as RecordBatchResult};
use common_recordbatch::{
    RecordBatch, RecordBatchStream, RecordBatches, SendableRecordBatchStream,
};
use datafusion_expr::Volatility;
use datatypes::schema::{ColumnSchema, SchemaRef};
use datatypes::vectors::VectorRef;
use futures::Stream;
use query::parser::{QueryLanguageParser, QueryStatement};
use query::QueryEngineRef;
use session::context::QueryContext;
use snafu::{ensure, ResultExt};
use sql::statements::statement::Statement;

use crate::engine::{CompileContext, EvalContext, Script, ScriptEngine};
use crate::python::error::{self, Result};
use crate::python::ffi_types::copr::{exec_parsed, parse, AnnotationInfo, CoprocessorRef};

const PY_ENGINE: &str = "python";

#[derive(Debug)]
pub struct PyUDF {
    copr: CoprocessorRef,
}

impl std::fmt::Display for PyUDF {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}({})->",
            &self.copr.name,
            self.copr
                .deco_args
                .arg_names
                .as_ref()
                .unwrap_or(&vec![])
                .join(",")
        )
    }
}

impl PyUDF {
    fn from_copr(copr: CoprocessorRef) -> Arc<Self> {
        Arc::new(Self { copr })
    }

    /// Register to `FUNCTION_REGISTRY`
    fn register_as_udf(zelf: Arc<Self>) {
        FUNCTION_REGISTRY.register(zelf)
    }
    fn register_to_query_engine(zelf: Arc<Self>, engine: QueryEngineRef) {
        engine.register_function(zelf)
    }

    /// Fake a schema, should only be used with dynamically eval a Python Udf
    fn fake_schema(&self, columns: &[VectorRef]) -> SchemaRef {
        let empty_args = vec![];
        let arg_names = self
            .copr
            .deco_args
            .arg_names
            .as_ref()
            .unwrap_or(&empty_args);
        let col_sch: Vec<_> = columns
            .iter()
            .enumerate()
            .map(|(i, col)| ColumnSchema::new(arg_names[i].clone(), col.data_type(), true))
            .collect();
        let schema = datatypes::schema::Schema::new(col_sch);
        Arc::new(schema)
    }
}

impl Function for PyUDF {
    fn name(&self) -> &str {
        &self.copr.name
    }

    fn return_type(
        &self,
        _input_types: &[datatypes::prelude::ConcreteDataType],
    ) -> common_query::error::Result<datatypes::prelude::ConcreteDataType> {
        // TODO(discord9): use correct return annotation if exist
        match self.copr.return_types.get(0) {
            Some(Some(AnnotationInfo {
                datatype: Some(ty), ..
            })) => Ok(ty.clone()),
            _ => PyUdfSnafu {
                msg: "Can't found return type for python UDF {self}",
            }
            .fail(),
        }
    }

    fn signature(&self) -> common_query::prelude::Signature {
        // try our best to get a type signature
        let mut arg_types = Vec::with_capacity(self.copr.arg_types.len());
        let mut know_all_types = true;
        for ty in self.copr.arg_types.iter() {
            match ty {
                Some(AnnotationInfo {
                    datatype: Some(ty), ..
                }) => arg_types.push(ty.clone()),
                _ => {
                    know_all_types = false;
                    break;
                }
            }
        }
        if know_all_types {
            Signature::variadic(arg_types, Volatility::Immutable)
        } else {
            Signature::any(self.copr.arg_types.len(), Volatility::Immutable)
        }
    }

    fn eval(
        &self,
        _func_ctx: common_function::scalars::function::FunctionContext,
        columns: &[datatypes::vectors::VectorRef],
    ) -> common_query::error::Result<datatypes::vectors::VectorRef> {
        // FIXME(discord9): exec_parsed require a RecordBatch(basically a Vector+Schema), where schema can't pop out from nowhere, right?
        let schema = self.fake_schema(columns);
        let columns = columns.to_vec();
        let rb = Some(RecordBatch::new(schema, columns).context(UdfTempRecordBatchSnafu)?);
        let res = exec_parsed(&self.copr, &rb, &HashMap::new()).map_err(|err| {
            PyUdfSnafu {
                msg: format!("{err:#?}"),
            }
            .build()
        })?;
        let len = res.columns().len();
        if len == 0 {
            return PyUdfSnafu {
                msg: "Python UDF should return exactly one column, found zero column".to_string(),
            }
            .fail();
        } // if more than one columns, just return first one

        // TODO(discord9): more error handling
        let res0 = res.column(0);
        Ok(res0.clone())
    }
}

pub struct PyScript {
    query_engine: QueryEngineRef,
    copr: CoprocessorRef,
}

impl PyScript {
    /// Register Current Script as UDF, register name is same as script name
    /// FIXME(discord9): possible inject attack?
    pub fn register_udf(&self) {
        let udf = PyUDF::from_copr(self.copr.clone());
        PyUDF::register_as_udf(udf.clone());
        PyUDF::register_to_query_engine(udf, self.query_engine.clone());
    }
}

pub struct CoprStream {
    stream: SendableRecordBatchStream,
    copr: CoprocessorRef,
    params: HashMap<String, String>,
}

impl RecordBatchStream for CoprStream {
    fn schema(&self) -> SchemaRef {
        self.stream.schema()
    }
}

impl Stream for CoprStream {
    type Item = RecordBatchResult<RecordBatch>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.stream).poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(recordbatch))) => {
                let batch = exec_parsed(&self.copr, &Some(recordbatch), &self.params)
                    .map_err(BoxedError::new)
                    .context(ExternalSnafu)?;

                Poll::Ready(Some(Ok(batch)))
            }
            Poll::Ready(other) => Poll::Ready(other),
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.stream.size_hint()
    }
}

#[async_trait]
impl Script for PyScript {
    type Error = error::Error;

    fn engine_name(&self) -> &str {
        PY_ENGINE
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    async fn execute(&self, params: HashMap<String, String>, _ctx: EvalContext) -> Result<Output> {
        if let Some(sql) = &self.copr.deco_args.sql {
            let stmt = QueryLanguageParser::parse_sql(sql).unwrap();
            ensure!(
                matches!(stmt, QueryStatement::Sql(Statement::Query { .. })),
                error::UnsupportedSqlSnafu { sql }
            );
            let plan = self
                .query_engine
                .statement_to_plan(stmt, Arc::new(QueryContext::new()))
                .await?;
            let res = self.query_engine.execute(&plan).await?;
            let copr = self.copr.clone();
            match res {
                Output::Stream(stream) => Ok(Output::Stream(Box::pin(CoprStream {
                    params,
                    copr,
                    stream,
                }))),
                _ => unreachable!(),
            }
        } else {
            let batch = exec_parsed(&self.copr, &None, &params)?;
            let batches = RecordBatches::try_new(batch.schema.clone(), vec![batch]).unwrap();
            Ok(Output::RecordBatches(batches))
        }
    }
}

pub struct PyEngine {
    query_engine: QueryEngineRef,
}

impl PyEngine {
    pub fn new(query_engine: QueryEngineRef) -> Self {
        Self { query_engine }
    }
}

#[async_trait]
impl ScriptEngine for PyEngine {
    type Error = error::Error;
    type Script = PyScript;

    fn name(&self) -> &str {
        PY_ENGINE
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    async fn compile(&self, script: &str, _ctx: CompileContext) -> Result<PyScript> {
        let copr = Arc::new(parse::parse_and_compile_copr(
            script,
            Some(self.query_engine.clone()),
        )?);

        Ok(PyScript {
            copr,
            query_engine: self.query_engine.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use catalog::local::{MemoryCatalogProvider, MemorySchemaProvider};
    use catalog::{CatalogList, CatalogProvider, SchemaProvider};
    use common_catalog::consts::{DEFAULT_CATALOG_NAME, DEFAULT_SCHEMA_NAME};
    use common_recordbatch::util;
    use datatypes::prelude::ScalarVector;
    use datatypes::value::Value;
    use datatypes::vectors::{Float64Vector, Int64Vector};
    use query::QueryEngineFactory;
    use table::table::numbers::NumbersTable;

    use super::*;

    fn sample_script_engine() -> PyEngine {
        let catalog_list = catalog::local::new_memory_catalog_list().unwrap();

        let default_schema = Arc::new(MemorySchemaProvider::new());
        default_schema
            .register_table("numbers".to_string(), Arc::new(NumbersTable::default()))
            .unwrap();
        let default_catalog = Arc::new(MemoryCatalogProvider::new());
        default_catalog
            .register_schema(DEFAULT_SCHEMA_NAME.to_string(), default_schema)
            .unwrap();
        catalog_list
            .register_catalog(DEFAULT_CATALOG_NAME.to_string(), default_catalog)
            .unwrap();

        let factory = QueryEngineFactory::new(catalog_list);
        let query_engine = factory.query_engine();

        PyEngine::new(query_engine.clone())
    }

    #[tokio::test]
    async fn test_sql_in_py() {
        let script_engine = sample_script_engine();

        let script = r#"
import greptime as gt

@copr(args=["number"], returns = ["number"], sql = "select * from numbers")
def test(number)->vector[u32]:
    return query.sql("select * from numbers")[0][0]
"#;
        let script = script_engine
            .compile(script, CompileContext::default())
            .await
            .unwrap();
        let output = script
            .execute(HashMap::default(), EvalContext::default())
            .await
            .unwrap();
        let res = common_recordbatch::util::collect_batches(match output {
            Output::Stream(s) => s,
            _ => unreachable!(),
        })
        .await
        .unwrap();
        let rb = res.iter().next().expect("One and only one recordbatch");
        assert_eq!(rb.column(0).len(), 100);
    }

    #[tokio::test]
    async fn test_user_params_in_py() {
        let script_engine = sample_script_engine();

        let script = r#"
@copr(returns = ["number"])
def test(**params)->vector[i64]:
    return int(params['a']) + int(params['b'])
"#;
        let script = script_engine
            .compile(script, CompileContext::default())
            .await
            .unwrap();
        let mut params = HashMap::new();
        params.insert("a".to_string(), "30".to_string());
        params.insert("b".to_string(), "12".to_string());
        let _output = script
            .execute(params, EvalContext::default())
            .await
            .unwrap();
        let res = match _output {
            Output::RecordBatches(s) => s,
            _ => todo!(),
        };
        let rb = res.iter().next().expect("One and only one recordbatch");
        assert_eq!(rb.column(0).len(), 1);
        let result = rb.column(0).get(0);
        assert!(matches!(result, Value::Int64(42)));
    }

    #[tokio::test]
    async fn test_data_frame_in_py() {
        let script_engine = sample_script_engine();

        let script = r#"
import greptime as gt
from data_frame import col

@copr(args=["number"], returns = ["number"], sql = "select * from numbers")
def test(number)->vector[u32]:
    return dataframe.filter(col("number")==col("number")).collect()[0][0]
"#;
        let script = script_engine
            .compile(script, CompileContext::default())
            .await
            .unwrap();
        let _output = script
            .execute(HashMap::new(), EvalContext::default())
            .await
            .unwrap();
        let res = common_recordbatch::util::collect_batches(match _output {
            Output::Stream(s) => s,
            _ => todo!(),
        })
        .await
        .unwrap();
        let rb = res.iter().next().expect("One and only one recordbatch");
        assert_eq!(rb.column(0).len(), 100);
    }

    #[tokio::test]
    async fn test_compile_execute() {
        let script_engine = sample_script_engine();

        // To avoid divide by zero, the script divides `add(a, b)` by `g.sqrt(c + 1)` instead of `g.sqrt(c)`
        let script = r#"
import greptime as g
def add(a, b):
    return a + b;

@copr(args=["a", "b", "c"], returns = ["r"], sql="select number as a,number as b,number as c from numbers limit 100")
def test(a, b, c):
    return add(a, b) / g.sqrt(c + 1)
"#;
        let script = script_engine
            .compile(script, CompileContext::default())
            .await
            .unwrap();
        let output = script
            .execute(HashMap::new(), EvalContext::default())
            .await
            .unwrap();
        match output {
            Output::Stream(stream) => {
                let numbers = util::collect(stream).await.unwrap();

                assert_eq!(1, numbers.len());
                let number = &numbers[0];
                assert_eq!(number.num_columns(), 1);
                assert_eq!("r", number.schema.column_schemas()[0].name);

                assert_eq!(1, number.num_columns());
                assert_eq!(100, number.column(0).len());
                let rows = number
                    .column(0)
                    .as_any()
                    .downcast_ref::<Float64Vector>()
                    .unwrap();
                assert_eq!(0f64, rows.get_data(0).unwrap());
                assert_eq!((99f64 + 99f64) / 100f64.sqrt(), rows.get_data(99).unwrap())
            }
            _ => unreachable!(),
        }

        // test list comprehension
        let script = r#"
import greptime as gt

@copr(args=["number"], returns = ["r"], sql="select number from numbers limit 100")
def test(a):
    return gt.vector([x for x in a if x % 2 == 0])
"#;
        let script = script_engine
            .compile(script, CompileContext::default())
            .await
            .unwrap();
        let output = script
            .execute(HashMap::new(), EvalContext::default())
            .await
            .unwrap();
        match output {
            Output::Stream(stream) => {
                let numbers = util::collect(stream).await.unwrap();

                assert_eq!(1, numbers.len());
                let number = &numbers[0];
                assert_eq!(number.num_columns(), 1);
                assert_eq!("r", number.schema.column_schemas()[0].name);

                assert_eq!(1, number.num_columns());
                assert_eq!(50, number.column(0).len());
                let rows = number
                    .column(0)
                    .as_any()
                    .downcast_ref::<Int64Vector>()
                    .unwrap();
                assert_eq!(0, rows.get_data(0).unwrap());
                assert_eq!(98, rows.get_data(49).unwrap())
            }
            _ => unreachable!(),
        }
    }
}
