#[macro_use]
extern crate nom;

#[macro_use]
#[cfg(test)]
extern crate maplit;

mod data;
mod operator;
mod lang;
mod render;

pub mod pipeline {
    use lang;
    use operator;
    use lang::*;
    use render::{RenderConfig, Renderer};
    use data::{Record, Row};
    use std::io::BufRead;

    pub struct Pipeline {
        filter: lang::Search,
        pre_aggregates: Vec<Box<operator::UnaryPreAggOperator>>,
        aggregators: Vec<Box<operator::AggregateOperator>>,
        renderer: Renderer,
    }

    impl Pipeline {
        fn convert_inline(op: lang::InlineOperator) -> Box<operator::UnaryPreAggOperator> {
            match op {
                InlineOperator::Json => Box::new(operator::ParseJson {}),
                InlineOperator::Parse { pattern, fields } => {
                    Box::new(operator::Parse::new(&pattern, fields).unwrap())
                }
            }
        }

        fn convert_agg(op: lang::AggregateOperator) -> Box<operator::AggregateOperator> {
            match op.aggregate_function {
                AggregateFunction::Count => Box::new(operator::Grouper::<operator::Count>::new(
                    op.key_cols.iter().map(AsRef::as_ref).collect(),
                    &op.output_column.unwrap_or("_count".to_string()),
                    operator::Count::new(),
                )),
                AggregateFunction::Average { column } => {
                    Box::new(operator::Grouper::<operator::Average>::new(
                        op.key_cols.iter().map(AsRef::as_ref).collect(),
                        &op.output_column.unwrap_or("_average".to_string()),
                        operator::Average::empty(column),
                    ))
                }
                AggregateFunction::Sum { column } => {
                    Box::new(operator::Grouper::<operator::Sum>::new(
                        op.key_cols.iter().map(AsRef::as_ref).collect(),
                        &op.output_column.unwrap_or("_sum".to_string()),
                        operator::Sum::empty(column),
                    ))
                }
                AggregateFunction::Percentile {
                    column,
                    percentile,
                    percentile_str,
                } => {
                    let column_name = format!("_p{}", percentile_str);
                    Box::new(operator::Grouper::<operator::Percentile>::new(
                        op.key_cols.iter().map(AsRef::as_ref).collect(),
                        &op.output_column.unwrap_or(column_name),
                        operator::Percentile::empty(column, percentile),
                    ))
                }
            }
        }

        pub fn new(pipeline: &str) -> Result<Self, String> {
            let fixed_pipeline = format!("{}!", pipeline); // todo: fix hack
            let parsed = lang::parse_query(&fixed_pipeline);
            let query = match parsed {
                Ok((_input, query)) => query,
                Err(s) => return Result::Err(format!("Could not parse query: {:?}", s)),
            };
            let mut in_agg = false;
            let mut pre_agg: Vec<Box<operator::UnaryPreAggOperator>> = Vec::new();
            let mut post_agg: Vec<Box<operator::AggregateOperator>> = Vec::new();
            for op in query.operators {
                match op {
                    Operator::Inline(inline_op) => if !in_agg {
                        pre_agg.push(Pipeline::convert_inline(inline_op));
                    } else {
                        return Result::Err("non aggregate cannot follow aggregate".to_string());
                    },
                    Operator::Aggregate(agg_op) => {
                        in_agg = true;
                        post_agg.push(Pipeline::convert_agg(agg_op))
                    }
                }
            }
            Result::Ok(Pipeline {
                filter: query.search,
                pre_aggregates: pre_agg,
                aggregators: post_agg,
                renderer: Renderer::new(RenderConfig {
                    floating_points: 2,
                    min_buffer: 4,
                    max_buffer: 8,
                }),
            })
        }

        fn matches(&self, rec: &Record) -> bool {
            match &self.filter {
                &lang::Search::MatchAll => true,
                &lang::Search::MatchFilter(ref filter) => rec.raw.contains(filter),
            }
        }

        pub fn process<T: BufRead>(&mut self, buf: T) {
            for line in buf.lines() {
                self.proc_str(&(line.unwrap()));
            }
        }

        fn proc_str(&mut self, s: &str) {
            let mut rec = Record::new(s);
            if self.matches(&rec) {
                for pre_agg in &self.pre_aggregates {
                    match (*pre_agg).process(&rec) {
                        Some(next_rec) => rec = next_rec,
                        None => return,
                    }
                }

                let mut row = Row::Record(rec);
                for agg in self.aggregators.iter_mut() {
                    (*agg).process(&row);
                    row = Row::Aggregate((*agg).emit());
                }
                self.renderer.render(row);
            }
        }
    }
}
