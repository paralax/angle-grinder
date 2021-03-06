extern crate itertools;
extern crate ord_subset;
extern crate quantiles;
extern crate regex;
extern crate regex_syntax;
extern crate serde_json;
use std::collections::HashMap;
use std::iter::FromIterator;
use self::ord_subset::OrdSubsetSliceExt;
use data::{Aggregate, Record, Row};
use data;
use self::serde_json::Value;
use self::quantiles::ckms::CKMS;
pub trait UnaryPreAggOperator {
    fn process(&self, rec: &Record) -> Option<Record>;
}

pub trait AggregateOperator {
    fn emit(&self) -> data::Aggregate;
    fn process(&mut self, row: &Row);
}

pub trait AggregateFunction: Clone {
    fn process(&mut self, rec: &Record);
    fn emit(&self) -> data::Value;
}

#[derive(Clone)]
pub struct Count {
    count: i64,
}

impl Count {
    pub fn new() -> Count {
        Count { count: 0 }
    }
}

impl AggregateFunction for Count {
    fn process(&mut self, _rec: &Record) {
        self.count += 1;
    }

    fn emit(&self) -> data::Value {
        data::Value::Int(self.count)
    }
}

#[derive(Clone)]
pub struct Sum {
    total: f64,
    column: String,
    warnings: Vec<String>,
    is_float: bool,
}

impl Sum {
    pub fn empty(column: String) -> Self {
        Sum {
            total: 0.0,
            column: column,
            warnings: Vec::new(),
            is_float: false,
        }
    }
}

impl AggregateFunction for Sum {
    fn process(&mut self, rec: &Record) {
        rec.data
            .get(&self.column)
            .iter()
            .for_each(|value| match value {
                &&data::Value::Float(ref f) => {
                    self.is_float = true;
                    self.total += f;
                }
                &&data::Value::Int(ref i) => {
                    self.total += *i as f64;
                }
                _other => self.warnings
                    .push("Got string. Can only average into or float".to_string()),
            });
    }

    fn emit(&self) -> data::Value {
        data::Value::Int(self.total as i64)
    }
}

#[derive(Clone)]
pub struct Average {
    total: f64,
    count: i64,
    column: String,
    warnings: Vec<String>,
}

impl Average {
    pub fn empty(column: String) -> Average {
        Average {
            total: 0.0,
            count: 0,
            column: column,
            warnings: Vec::new(),
        }
    }
}

impl AggregateFunction for Average {
    fn process(&mut self, rec: &Record) {
        rec.data
            .get(&self.column)
            .iter()
            .for_each(|value| match value {
                &&data::Value::Float(ref f) => {
                    self.total += f;
                    self.count += 1
                }
                &&data::Value::Int(ref i) => {
                    self.total += *i as f64;
                    self.count += 1
                }
                _other => self.warnings
                    .push("Got string. Can only average into or float".to_string()),
            });
    }

    fn emit(&self) -> data::Value {
        data::Value::Float(self.total / self.count as f64)
    }
}

#[derive(Clone)]
pub struct Percentile {
    ckms: CKMS<f64>,
    column: String,
    percentile: f64,
    warnings: Vec<String>,
}

impl Percentile {
    pub fn empty(column: String, percentile: f64) -> Self {
        if percentile >= 1.0 {
            panic!("Percentiles must be < 1");
        }

        Percentile {
            ckms: CKMS::<f64>::new(0.001),
            column: column,
            warnings: Vec::new(),
            percentile: percentile,
        }
    }
}

impl AggregateFunction for Percentile {
    fn process(&mut self, rec: &Record) {
        rec.data
            .get(&self.column)
            .iter()
            .for_each(|value| match value {
                &&data::Value::Float(ref f) => {
                    self.ckms.insert(*f);
                }
                &&data::Value::Int(ref i) => self.ckms.insert(*i as f64),
                _other => self.warnings
                    .push("Got string. Can only average int or float".to_string()),
            });
    }

    fn emit(&self) -> data::Value {
        let pct_opt = self.ckms.query(self.percentile);
        pct_opt
            .map(|(_usize, pct_float)| data::Value::Float(pct_float))
            .unwrap_or(data::Value::None)
    }
}

pub struct Grouper<T: AggregateFunction> {
    key_cols: Vec<String>,
    agg_col: String,
    state: HashMap<Vec<String>, T>,
    empty: T,
}

impl<T: AggregateFunction> Grouper<T> {
    pub fn new(key_cols: Vec<&str>, agg_col: &str, empty: T) -> Grouper<T> {
        Grouper {
            key_cols: key_cols.iter().map(|s| s.to_string()).collect(),
            agg_col: String::from(agg_col),
            state: HashMap::new(),
            empty: empty,
        }
    }
}

impl<T: AggregateFunction> AggregateOperator for Grouper<T> {
    fn emit(&self) -> data::Aggregate {
        let mut data: Vec<(HashMap<String, String>, data::Value)> = self.state
            .iter()
            .map(|(ref key_cols, ref accum)| {
                let key_value =
                    itertools::zip_eq(self.key_cols.iter().cloned(), key_cols.iter().cloned());
                let res_map: HashMap<String, String> = HashMap::from_iter(key_value);
                (res_map, accum.emit())
            })
            .collect(); // todo: avoid clone here
        data.ord_subset_sort_by_key(|ref kv| kv.1.clone());
        data.reverse();
        Aggregate::new(self.key_cols.clone(), self.agg_col.clone(), data)
    }

    fn process(&mut self, row: &Row) {
        match row {
            &Row::Record(ref rec) => {
                let key_values: Vec<Option<&data::Value>> = self.key_cols
                    .iter()
                    .cloned()
                    .map(|column| rec.data.get(&column))
                    .collect();
                let key_columns: Vec<String> = key_values
                    .iter()
                    .cloned()
                    .map(|value_opt| {
                        value_opt
                            .map(|value| value.to_string())
                            .unwrap_or("$None$".to_string())
                    })
                    .collect();
                let accum = self.state.entry(key_columns).or_insert(self.empty.clone());
                accum.process(rec);
            }
            &Row::Aggregate(ref _ag) => panic!("Unsupported"),
        }
    }
}

pub struct Parse {
    regex: regex::Regex,
    fields: Vec<String>,
}

impl Parse {
    pub fn new(pattern: &str, fields: Vec<String>) -> Result<Self, String> {
        let regex_str = regex_syntax::quote(pattern);
        let mut regex_str = regex_str.replace("\\*", "(.*?)");
        // If it ends with a star, we need to ensure we read until the end.
        if pattern.ends_with("*") {
            regex_str = format!("{}$", regex_str);
        }
        if pattern.matches('*').count() != fields.len() {
            Result::Err("Wrong number of extractions".to_string())
        } else {
            Result::Ok(Parse {
                regex: regex::Regex::new(&regex_str).unwrap(),
                fields: fields,
            })
        }
    }
}

impl UnaryPreAggOperator for Parse {
    fn process(&self, rec: &Record) -> Option<Record> {
        let matches: Vec<regex::Captures> = self.regex.captures_iter(&rec.raw).collect();
        if matches.len() == 0 {
            None
        } else {
            let capture = &matches[0];
            let mut rec = rec.clone();
            for i in 0..self.fields.len() {
                // the first capture is the entire string
                rec = rec.put(&self.fields[i], data::Value::from_string(&capture[i + 1]));
            }
            Some(rec)
        }
    }
}

pub struct ParseJson {
        // any options here
}
impl UnaryPreAggOperator for ParseJson {
    fn process(&self, rec: &Record) -> Option<Record> {
        match serde_json::from_str(&rec.raw) {
            Ok(v) => {
                let v: Value = v;
                let rec: Record = rec.clone();
                match v {
                    Value::Object(map) => {
                        map.iter().fold(Some(rec), |record_opt, (ref k, ref v)| {
                            record_opt.and_then(|record| match v {
                                &&Value::Number(ref num) => if num.is_i64() {
                                    Some(record.put(k, data::Value::Int(num.as_i64().unwrap())))
                                } else {
                                    Some(record.put(k, data::Value::Float(num.as_f64().unwrap())))
                                },
                                &&Value::String(ref s) => {
                                    Some(record.put(k, data::Value::Str(s.to_string())))
                                }
                                &&Value::Null => Some(record.put(k, data::Value::None)),
                                _other => None,
                            })
                        })
                    }
                    _other => None,
                }
            }
            _e => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use data::Record;
    use data::Value;
    use super::*;
    use operator::itertools::Itertools;

    #[test]
    fn test_json() {
        let rec = Record::new(r#"{"k1": 5, "k2": 5.5, "k3": "str", "k4": null}"#);
        let parser = ParseJson {};
        let rec = parser.process(&rec).unwrap();
        assert_eq!(
            rec.data,
            hashmap!{
                "k1".to_string() => Value::Int(5),
                "k2".to_string() => Value::Float(5.5),
                "k3".to_string() => Value::Str("str".to_string()),
                "k4".to_string() => Value::None
            }
        );
    }

    #[test]
    fn test_parse() {
        let rec = Record::new("17:12:14.214111 IP 10.0.2.243.53938 > taotie.canonical.com.http: Flags [.], ack 56575, win 2375, options [nop,nop,TS val 13651369 ecr 169698010], length 99");
        let parser = Parse::new(
            "IP * > *: * length *",
            vec![
                "sender".to_string(),
                "recip".to_string(),
                "ignore".to_string(),
                "length".to_string(),
            ],
        ).unwrap();
        let rec = parser.process(&rec).unwrap();
        assert_eq!(
            rec.data.get("sender").unwrap(),
            &Value::Str("10.0.2.243.53938".to_string())
        );
        assert_eq!(rec.data.get("length").unwrap(), &Value::Int(99));
        assert_eq!(
            rec.data.get("recip").unwrap(),
            &Value::Str("taotie.canonical.com.http".to_string())
        )
    }

    #[test]
    fn test_count_no_groups() {
        let mut count_agg = Grouper::<Count>::new(Vec::new(), "_count", Count::new());
        (0..10)
            .map(|n| Record::new(&n.to_string()))
            .foreach(|rec| count_agg.process(&Row::Record(rec)));
        let agg = count_agg.emit();
        assert_eq!(agg.key_columns.len(), 0);
        assert_eq!(agg.agg_column, "_count");
        assert_eq!(agg.data, vec![(HashMap::new(), data::Value::Int(10))]);
        (0..10)
            .map(|n| Record::new(&n.to_string()))
            .foreach(|rec| count_agg.process(&Row::Record(rec)));
        assert_eq!(
            count_agg.emit().data,
            vec![(HashMap::new(), data::Value::Int(20))]
        );
    }

    #[test]
    fn test_count_groups() {
        let mut count_agg = Grouper::<Count>::new(vec!["k1"], "_count", Count::new());
        (0..10).foreach(|n| {
            let rec = Record::new(&n.to_string());
            let rec = rec.put("k1", data::Value::Str("ok".to_string()));
            count_agg.process(&Row::Record(rec));
        });
        (0..25).foreach(|n| {
            let rec = Record::new(&n.to_string());
            let rec = rec.put("k1", data::Value::Str("not ok".to_string()));
            count_agg.process(&Row::Record(rec));
        });
        (0..3).foreach(|n| {
            let rec = Record::new(&n.to_string());
            count_agg.process(&Row::Record(rec));
        });
        let agg = count_agg.emit();
        assert_eq!(
            agg.data,
            vec![
                (
                    hashmap!{"k1".to_string() => "not ok".to_string()},
                    data::Value::Int(25),
                ),
                (
                    hashmap!{"k1".to_string() => "ok".to_string()},
                    data::Value::Int(10),
                ),
                (
                    hashmap!{"k1".to_string() => "$None$".to_string()},
                    data::Value::Int(3),
                ),
            ]
        );
    }
}
