// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::HashMap;
use std::collections::BTreeMap;
use gmp::mpq::Mpq;
use chrono::{DateTime, FixedOffset};
use number::{Number, Unit};
use date;
use ast::{DatePattern, Expr, SuffixOp, Def, Defs, Query, Conversion};
use std::ops::{Add, Div, Mul, Neg, Sub};
use std::rc::Rc;
use factorize::{factorize, Factors};

#[derive(Clone)]
pub enum Value {
    Number(Number),
    DateTime(DateTime<FixedOffset>),
}

/// The evaluation context that contains unit definitions.
#[derive(Debug)]
pub struct Context {
    pub dimensions: Vec<Rc<String>>,
    pub units: HashMap<String, Number>,
    pub aliases: HashMap<Unit, String>,
    pub reverse: HashMap<Unit, String>,
    pub prefixes: Vec<(String, Number)>,
    pub definitions: HashMap<String, Expr>,
    pub datepatterns: Vec<Vec<DatePattern>>,
    pub short_output: bool,
}

pub trait Show {
    /// Provides a string representation of something, using information contained in a Context.
    fn show(&self, context: &Context) -> String;
}

#[cfg(feature = "chrono-humanize")]
impl Show for DateTime<FixedOffset> {
    fn show(&self, _context: &Context) -> String {
        use chrono_humanize::HumanTime;
        format!("{} ({})", self, HumanTime::from(*self))
    }
}

#[cfg(not(feature = "chrono-humanize"))]
impl Show for DateTime<FixedOffset> {
    fn show(&self, _context: &Context) -> String {
        format!("{}", self)
    }
}

impl Show for Value {
    fn show(&self, context: &Context) -> String {
        match *self {
            Value::Number(ref num) => num.show(context),
            Value::DateTime(ref dt) => dt.show(context),
        }
    }
}

impl Value {
    fn pow(&self, exp: &Value) -> Result<Value, String> {
        match (self, exp) {
            (&Value::Number(ref left), &Value::Number(ref right)) =>
                left.pow(right).map(Value::Number),
            (_, _) => Err(format!("Operation is not defined"))
        }
    }
}

impl<'a,'b> Add<&'b Value> for &'a Value {
    type Output = Result<Value, String>;

    fn add(self, other: &Value) -> Result<Value, String> {
        match (self, other) {
            (&Value::Number(ref left), &Value::Number(ref right)) =>
                (left + right)
                .ok_or(format!("Addition of units with mismatched units is not meaningful"))
                .map(Value::Number),
            (&Value::DateTime(ref left), &Value::Number(ref right)) |
            (&Value::Number(ref right), &Value::DateTime(ref left)) =>
                left.checked_add(try!(date::to_duration(right)))
                .ok_or(format!("Implementation error: value is out of range representable by datetime"))
                .map(Value::DateTime),
            (_, _) => Err(format!("Operation is not defined"))
        }
    }
}

impl<'a,'b> Sub<&'b Value> for &'a Value {
    type Output = Result<Value, String>;

    fn sub(self, other: &Value) -> Result<Value, String> {
        match (self, other) {
            (&Value::Number(ref left), &Value::Number(ref right)) =>
                (left - right)
                .ok_or(format!("Subtraction of units with mismatched units is not meaningful"))
                .map(Value::Number),
            (&Value::DateTime(ref left), &Value::Number(ref right)) |
            (&Value::Number(ref right), &Value::DateTime(ref left)) =>
                left.checked_sub(try!(date::to_duration(right)))
                .ok_or(format!("Implementation error: value is out of range representable by datetime"))
                .map(Value::DateTime),
            (&Value::DateTime(ref left), &Value::DateTime(ref right)) =>
                date::from_duration(&(*left - *right))
                .map(Value::Number),
            //(_, _) => Err(format!("Operation is not defined"))
        }
    }
}

impl<'a> Neg for &'a Value {
    type Output = Result<Value, String>;

    fn neg(self) -> Self::Output {
        match *self {
            Value::Number(ref num) =>
                (-num).ok_or(format!("Bug: Negation should not fail")).map(Value::Number),
            _ => Err(format!("Operation is not defined"))
        }
    }
}

impl<'a,'b> Mul<&'b Value> for &'a Value {
    type Output = Result<Value, String>;

    fn mul(self, other: &Value) -> Result<Value, String> {
        match (self, other) {
            (&Value::Number(ref left), &Value::Number(ref right)) =>
                (left * right)
                .ok_or(format!("Bug: Mul should not fail"))
                .map(Value::Number),
            (_, _) => Err(format!("Operation is not defined"))
        }
    }
}

impl<'a,'b> Div<&'b Value> for &'a Value {
    type Output = Result<Value, String>;

    fn div(self, other: &Value) -> Result<Value, String> {
        match (self, other) {
            (&Value::Number(ref left), &Value::Number(ref right)) =>
                (left / right)
                .ok_or(format!("Division by zero"))
                .map(Value::Number),
            (_, _) => Err(format!("Operation is not defined"))
        }
    }
}

impl Context {
    /// Wrapper around show that calls `println!`.
    pub fn print(&self, value: &Number) {
        println!("{}", value.show(self));
    }

    /// Given a unit name, returns its value if it exists. Supports SI
    /// prefixes, plurals, bare dimensions like length, and aliases.
    pub fn lookup(&self, name: &str) -> Option<Number> {
        for k in &self.dimensions {
            if name == &***k {
                return Some(Number::one_unit(k.to_owned()))
            }
        }
        if let Some(v) = self.units.get(name).cloned() {
            return Some(v)
        }
        for (unit, alias) in &self.aliases {
            if name == alias {
                return Some(Number(Number::one().0, unit.clone()))
            }
        }
        if name.ends_with("s") {
            if let Some(v) = self.lookup(&name[0..name.len()-1]) {
                return Some(v)
            }
        }
        for &(ref pre, ref value) in &self.prefixes {
            if name.starts_with(pre) {
                if let Some(v) = self.lookup(&name[pre.len()..]) {
                    return Some((&v * &value).unwrap())
                }
            }
        }
        None
    }

    /// Describes a value's unit, gives true if the unit is reciprocal
    /// (e.g. you should prefix "1.0 / " or replace "multiply" with
    /// "divide" when rendering it).
    pub fn describe_unit(&self, value: &Number) -> (bool, String) {
        use std::io::Write;

        let mut buf = vec![];
        let mut recip = false;
        let square = Number(Mpq::one(), value.1.clone()).root(2);
        let inverse = (&Number::one() / &Number(Mpq::one(), value.1.clone())).unwrap();
        if let Some(name) = self.aliases.get(&value.1) {
            write!(buf, "{}", name).unwrap();
        } else if let Some(name) = square.and_then(|square| self.aliases.get(&square.1)) {
            write!(buf, "{}^2", name).unwrap();
        } else if let Some(name) = self.aliases.get(&inverse.1) {
            recip = true;
            write!(buf, "{}", name).unwrap();
        } else {
            let mut frac = vec![];
            let mut found = false;
            for (dim, &pow) in &value.1 {
                if pow < 0 {
                    frac.push((dim, -pow));
                } else {
                    found = true;
                    let mut map = Unit::new();
                    map.insert(dim.clone(), pow);
                    if let Some(name) = self.aliases.get(&map) {
                        write!(buf, " {}", name).unwrap();
                    } else {
                        let mut map = Unit::new();
                        map.insert(dim.clone(), 1);
                        if let Some(name) = self.aliases.get(&map) {
                            write!(buf, " {}", name).unwrap();
                        } else {
                            write!(buf, " '{}'", dim).unwrap();
                        }
                        if pow != 1 {
                            write!(buf, "^{}", pow).unwrap();
                        }
                    }
                }
            }
            if frac.len() > 0 {
                if !found {
                    recip = true;
                } else {
                    write!(buf, " /").unwrap();
                }
                for (dim, pow) in frac {
                    let mut map = Unit::new();
                    map.insert(dim.clone(), pow);
                    if let Some(name) = self.aliases.get(&map) {
                        write!(buf, " {}", name).unwrap();
                    } else {
                        let mut map = Unit::new();
                        map.insert(dim.clone(), 1);
                        if let Some(name) = self.aliases.get(&map) {
                            write!(buf, " {}", name).unwrap();
                        } else {
                            write!(buf, " '{}'", dim).unwrap();
                        }
                        if pow != 1 {
                            write!(buf, "^{}", pow).unwrap();
                        }
                    }
                }
            }
            buf.remove(0);
        }

        (recip, String::from_utf8(buf).unwrap())
    }

    /// Evaluates an expression to compute its value, *excluding* `->`
    /// conversions.
    pub fn eval(&self, expr: &Expr) -> Result<Value, String> {
        macro_rules! operator {
            ($left:ident $op:ident $opname:tt $right:ident) => {{
                let left = try!(self.eval(&**$left));
                let right = try!(self.eval(&**$right));
                ((&left).$op(&right)).map_err(|e| {
                    format!("{}: <{}> {} <{}>",
                            e, left.show(self), stringify!($opname), right.show(self))
                })
            }}
        }

        macro_rules! temperature {
            ($left:ident, $name:expr, $base:expr, $scale:expr) => {{
                let left = try!(self.eval(&**$left));
                let left = match left {
                    Value::Number(left) => left,
                    _ => return Err(format!("Expected number, got: <{}> °{}",
                                            left.show(self), stringify!($name)))
                };
                if left.1 != BTreeMap::new() {
                    Err(format!("Expected dimensionless, got: <{}>", left.show(self)))
                } else {
                    let left = (&left * &self.lookup($scale).expect(&*format!("Missing {} unit", $scale))).unwrap();
                    Ok(Value::Number((&left + &self.lookup($base)
                                      .expect(&*format!("Missing {} constant", $base))).unwrap()))
                }
            }}
        }

        match *expr {
            Expr::Unit(ref name) if name == "now" => Ok(Value::DateTime(date::now())),
            Expr::Unit(ref name) => self.lookup(name).ok_or(format!("Unknown unit {}", name)).map(Value::Number),
            Expr::Quote(ref name) => Ok(Value::Number(Number::one_unit(Rc::new(name.clone())))),
            Expr::Const(ref num, ref frac, ref exp) =>
                Number::from_parts(
                    num,
                    frac.as_ref().map(AsRef::as_ref),
                    exp.as_ref().map(AsRef::as_ref))
                .map(Value::Number),
            Expr::Date(ref date) => date::try_decode(date, self).map(Value::DateTime),
            Expr::Neg(ref expr) => self.eval(&**expr).and_then(|v| -&v),
            Expr::Plus(ref expr) => self.eval(&**expr),

            Expr::Frac(ref left, ref right) => operator!(left div / right),
            Expr::Add(ref left, ref right)  => operator!(left add + right),
            Expr::Sub(ref left, ref right)  => operator!(left sub - right),
            Expr::Pow(ref left, ref right)  => operator!(left pow ^ right),

            Expr::Suffix(SuffixOp::Celsius, ref left) =>
                temperature!(left, "C", "zerocelsius", "kelvin"),
            Expr::Suffix(SuffixOp::Fahrenheit, ref left) =>
                temperature!(left, "F", "zerofahrenheit", "degrankine"),
            Expr::Suffix(SuffixOp::Reaumur, ref left) =>
                temperature!(left, "Ré", "zerocelsius", "reaumur_absolute"),
            Expr::Suffix(SuffixOp::Romer, ref left) =>
                temperature!(left, "Rø", "zeroromer", "romer_absolute"),
            Expr::Suffix(SuffixOp::Delisle, ref left) =>
                temperature!(left, "De", "zerodelisle", "delisle_absolute"),
            Expr::Suffix(SuffixOp::Newton, ref left) =>
                temperature!(left, "N", "zerocelsius", "newton_absolute"),

            // TODO: A type might not implement * on Number, and this would fail
            Expr::Mul(ref args) => args.iter().fold(Ok(Value::Number(Number::one())), |a, b| {
                a.and_then(|a| {
                    let b = try!(self.eval(b));
                    Ok((&a * &b).unwrap())
                })
            }),
            Expr::Equals(_, ref right) => self.eval(right),
            Expr::Call(ref name, ref args) => {
                let args = try!(args.iter().map(|x| self.eval(x)).collect::<Result<Vec<_>, _>>());
                match &**name {
                    "sqrt" => {
                        if args.len() != 1 {
                            return Err(format!("Argument number mismatch for sqrt: expected 1, got {}", args.len()))
                        }
                        match args[0] {
                            Value::Number(ref num) =>
                                num.root(2).map(Value::Number).ok_or(format!(
                                    "Expected squared units, got <{}>", num.show(self))),
                            ref x => Err(format!("Expected number, got <{}>", x.show(self)))
                        }
                    },
                    _ => Err(format!("Function not found: {}", name))
                }
            },
            Expr::Error(ref e) => Err(e.clone()),
        }
    }

    pub fn eval_unit_name(&self, expr: &Expr) -> Result<BTreeMap<String, isize>, String> {
        match *expr {
            Expr::Equals(ref left, ref _right) => match **left {
                Expr::Unit(ref name) => {
                    let mut map = BTreeMap::new();
                    map.insert(name.clone(), 1);
                    Ok(map)
                },
                ref x => Err(format!("Expected identifier, got {:?}", x))
            },
            Expr::Call(_, _) => Err(format!("Calls are not allowed in the right hand side of conversions")),
            Expr::Unit(ref name) | Expr::Quote(ref name) => {
                let mut map = BTreeMap::new();
                map.insert(name.clone(), 1);
                Ok(map)
            },
            Expr::Const(ref x, None, None) if x == "1" || x == "-1" => Ok(BTreeMap::new()),
            Expr::Const(_, _, _) => Err(format!("Constants are not allowed in the right hand side of conversions")),
            Expr::Frac(ref left, ref right) => {
                let left = try!(self.eval_unit_name(left));
                let right = try!(self.eval_unit_name(right)).into_iter()
                    .map(|(k,v)| (k, -v)).collect::<BTreeMap<_, _>>();
                Ok(::btree_merge(&left, &right, |a,b| if a+b != 0 { Some(a + b) } else { None }))
            },
            Expr::Mul(ref args) => {
                args[1..].iter().fold(self.eval_unit_name(&args[0]), |acc, b| {
                    let acc = try!(acc);
                    let b = try!(self.eval_unit_name(b));
                    Ok(::btree_merge(&acc, &b, |a,b| if a+b != 0 { Some(a+b) } else { None }))
                })
            },
            Expr::Pow(ref left, ref exp) => {
                let res = try!(self.eval(exp));
                let res = match res {
                    Value::Number(num) => num,
                    _ => return Err(format!("Exponents must be numbers"))
                };
                if res.1.len() > 0 {
                    return Err(format!("Exponents must be dimensionless"))
                }
                let res: f64 = res.0.into();
                Ok(try!(self.eval_unit_name(left)).into_iter()
                   .filter_map(|(k, v)| {
                       let v = v * res as isize;
                       if v != 0 {
                           Some((k, v))
                       } else {
                           None
                       }
                   })
                   .collect::<BTreeMap<_, _>>())
            },
            Expr::Add(ref left, ref right) | Expr::Sub(ref left, ref right) => {
                let left = try!(self.eval_unit_name(left));
                let right = try!(self.eval_unit_name(right));
                if left != right {
                    return Err(format!("Add of values with differing dimensions is not meaningful"))
                }
                Ok(left)
            },
            Expr::Neg(ref v) => self.eval_unit_name(v),
            Expr::Plus(ref v) => self.eval_unit_name(v),
            Expr::Suffix(_, _) =>
                Err(format!("Temperature conversions must not be compound units")),
            Expr::Date(_) => Err(format!("Dates are not allowed in the right hand side of conversions")),
            Expr::Error(ref e) => Err(e.clone()),
        }
    }

    /// Evaluates an expression, include `->` conversions.
    pub fn eval_outer(&self, expr: &Query) -> Result<String, String> {
        let conformance_err = |top: &Number, bottom: &Number| -> String {
            use std::io::Write;

            let mut buf = vec![];
            let width = 12;
            let mut topu = top.clone();
            topu.0 = Mpq::one();
            let mut bottomu = bottom.clone();
            bottomu.0 = Mpq::one();
            let left = topu.show(self);
            let right = bottomu.show(self);
            if self.short_output {
                writeln!(buf, "Conformance error [ {left} || {right} ]",
                         left=left, right=right).unwrap();
            } else {
                writeln!(buf, concat!("Conformance error\n",
                                      "{:>width$}: {left}\n",
                                      "{:>width$}: {right}"),
                         "Left side", "Right side", left=left, right=right, width=width).unwrap();
            }
            let diff = (&topu * &bottomu).unwrap();
            if diff.1.len() == 0 {
                writeln!(buf, "{:>width$}: Reciprocal conversion, invert one side",
                         "Suggestions", width=width).unwrap();
            } else {
                let diff = (&topu / &bottomu).unwrap();
                let (recip, desc) = self.describe_unit(&diff.invert());
                let word = match recip {
                    false => "multiply",
                    true => "divide"
                };
                writeln!(buf, "{:>width$}: {word} left side by {}", "Suggestions",
                         desc.trim(), width=width, word=word).unwrap();
                let (recip, desc) = self.describe_unit(&diff);
                let word = match recip {
                    false => "multiply",
                    true => "divide"
                };
                writeln!(buf, "{:>width$}  {word} right side by {}", "",
                         desc.trim(), width=width, word=word).unwrap();
            }

            String::from_utf8(buf).unwrap()
        };

        let show = |raw: &Number, bottom: &Number, bottom_name: BTreeMap<String, isize>| -> String {
            let number = raw.show_number_part();
            let mut unit_top = vec![];
            let mut unit_frac = vec![];
            for (name, exp) in bottom_name.into_iter() {
                if exp < 0 {
                    unit_frac.push((name, -exp));
                } else {
                    unit_top.push((name, exp));
                }
            }
            let unit_top = unit_top.into_iter().fold(String::new(), |mut acc, (name, exp)| {
                acc.push(' ');
                acc.push_str(&*name);
                if exp != 1 {
                    acc.push_str(&*format!("^{}", exp));
                }
                acc
            });
            let unit_frac = unit_frac.into_iter().fold(String::new(), |mut acc, (name, exp)| {
                if acc.len() > 0 {
                    acc.push(' ');
                }
                acc.push_str(&*name);
                if exp != 1 {
                    acc.push_str(&*format!("^{}", exp));
                }
                acc
            });
            let unit_frac = if unit_frac.len() > 0 {
                format!(" / {}", unit_frac)
            } else {
                unit_frac
            };
            let reduced = match self.describe_unit(&bottom) {
                (false, v) => v,
                (true, v) => format!("1 / {}", v)
            };
            format!("{number}{unit_top}{unit_frac} ({reduced})",
                    number=number, unit_top=unit_top,
                    unit_frac=unit_frac, reduced=reduced)
        };

        match *expr {
            Query::Expr(Expr::Unit(ref name)) if self.definitions.contains_key(name) => {
                let mut name = name;
                while let Some(&Expr::Unit(ref unit)) = self.definitions.get(name) {
                    if self.definitions.get(unit).is_none() {
                        break
                    }
                    name = unit;
                }
                let ref def = self.definitions[name];
                let res = self.lookup(name).unwrap();
                Ok(format!("Definition: {} = {} = {}", name, def, res.show(self)))
            },
            Query::Convert(ref top, Conversion::Expr(ref bottom)) => match (self.eval(top), self.eval(bottom), self.eval_unit_name(bottom)) {
                (Ok(top), Ok(bottom), Ok(bottom_name)) => {
                    let (top, bottom) = match (top, bottom) {
                        (Value::Number(top), Value::Number(bottom)) => (top, bottom),
                        _ => return Err(format!("Conversion of non-numbers is not defined"))
                    };
                    if top.1 == bottom.1 {
                        let raw = match &top / &bottom {
                            Some(raw) => raw,
                            None => return Err(format!("Division by zero: {} / {}",
                                                       top.show(self), bottom.show(self)))
                        };
                        Ok(show(&raw, &bottom, bottom_name))
                    } else {
                        Err(conformance_err(&top, &bottom))
                    }
                },
                (Err(e), _, _) => Err(e),
                (_, Err(e), _) => Err(e),
                (_, _, Err(e)) => Err(e),
            },
            Query::Convert(ref top, Conversion::List(ref list)) => {
                let top = try!(self.eval(top));
                let top = match top {
                    Value::Number(num) => num,
                    _ => return Err(format!("Cannot convert <{}> to {:?}",
                                            top.show(self), list))
                };
                let units = try!(list.iter().map(|x| {
                    match self.lookup(x) {
                        Some(x) => Ok(x),
                        None => Err(format!("Unit {} does not exist", x))
                    }
                }).collect::<Result<Vec<Number>, _>>());
                {
                    let first = try!(units.first().ok_or(format!("Expected non-empty unit list")));
                    try!(units.iter().skip(1).map(|x| {
                        if first.1 != x.1 {
                            Err(format!("Units in unit list must conform: <{}> ; <{}>",
                                        first.show(self), x.show(self)))
                        } else {
                            Ok(())
                        }
                    }).collect::<Result<Vec<()>, _>>());
                    if top.1 != first.1 {
                        return Err(conformance_err(&top, &first))
                    }
                }
                let mut value = top.0;
                let mut out = vec![];
                let len = units.len();
                for (i, unit) in units.into_iter().enumerate() {
                    // value -= unit * floor(value/unit)
                    use gmp::mpz::Mpz;

                    let res = &value / &unit.0;
                    let div = &res.get_num() / res.get_den();
                    let rem = &value - &(&unit.0 * &Mpq::ratio(&div, &Mpz::one()));
                    value = rem;
                    if i == len-1 {
                        out.push(res);
                    } else {
                        out.push(Mpq::ratio(&div, &Mpz::one()));
                    }
                }
                let mut buf = vec![];
                for (name, value) in list.into_iter().zip(out.into_iter()) {
                    use std::io::Write;
                    use number;

                    write!(buf, "{} {}, ", number::to_string(&value).1, name).unwrap();
                }
                buf.pop(); buf.pop();
                if let Some(res) = self.aliases.get(&top.1) {
                    use std::io::Write;
                    write!(buf, " ({})", res).unwrap();
                }
                Ok(String::from_utf8(buf).unwrap())
            },
            Query::Convert(ref top, ref which @ Conversion::DegC) |
            Query::Convert(ref top, ref which @ Conversion::DegF) |
            Query::Convert(ref top, ref which @ Conversion::DegN) |
            Query::Convert(ref top, ref which @ Conversion::DegRe) |
            Query::Convert(ref top, ref which @ Conversion::DegRo) |
            Query::Convert(ref top, ref which @ Conversion::DegDe) => {
                let top = try!(self.eval(top));
                macro_rules! temperature {
                    ($name:expr, $base:expr, $scale:expr) => {{
                        let top = match top {
                            Value::Number(ref num) => num,
                            _ => return Err(format!("Cannot convert <{}> to °{}",
                                                    top.show(self), $name))
                        };
                        let bottom = self.lookup($scale)
                            .expect(&*format!("Unit {} missing", $scale));
                        if top.1 != bottom.1 {
                            Err(conformance_err(&top, &bottom))
                        } else {
                            let res = (top - &self.lookup($base)
                                       .expect(&*format!("Constant {} missing", $base))).unwrap();
                            let res = (&res / &bottom).unwrap();
                            let mut name = BTreeMap::new();
                            name.insert(format!("°{}", $name), 1);
                            Ok(show(&res, &bottom, name))
                        }
                    }}
                }

                match *which {
                    Conversion::DegC => temperature!("C", "zerocelsius", "kelvin"),
                    Conversion::DegF => temperature!("F", "zerofahrenheit", "degrankine"),
                    Conversion::DegRe => temperature!("Ré", "zerocelsius", "reaumur_absolute"),
                    Conversion::DegRo => temperature!("Rø", "zeroromer", "romer_absolute"),
                    Conversion::DegDe => temperature!("De", "zerodelisle", "delisle_absolute"),
                    Conversion::DegN => temperature!("N", "zerocelsius", "newton_absolute"),
                    _ => panic!()
                }
            },
            Query::Factorize(ref expr) => {
                let val = try!(self.eval(expr));
                let val = match val {
                    Value::Number(val) => val,
                    _ => return Err(format!("Cannot find derivatives of <{}>", val.show(self))),
                };
                let aliases = self.aliases.iter()
                    .map(|(a, b)| (a.clone(), Rc::new(b.clone())))
                    .collect::<BTreeMap<_, _>>();
                let results = factorize(&val, &aliases);
                let mut results = results.into_sorted_vec();
                results.dedup();
                let results = results.into_iter().map(|Factors(_score, names)| {
                    let mut next = BTreeMap::<Rc<String>, usize>::new();
                    for name in names.into_iter() {
                        *next.entry(name).or_insert(0) += 1;
                    }
                    let names = next.into_iter().map(|(a, b)| if b > 1 {
                        Rc::new(format!("{}^{}", a, b))
                    } else {
                        a
                    }).collect::<Vec<_>>();
                    let first = names.first().cloned();
                    names.into_iter().skip(1).fold(
                        first.map(|x| (**x).to_owned()).unwrap_or(String::new()),
                        |a, x| format!("{} {}", a, x))
                }).collect::<Vec<_>>();
                let first = results.first().cloned();
                let len = results.len();
                let results = results.into_iter().skip(1).fold(
                    first.unwrap_or(String::new()),
                    |a, x| format!("{};  {}", a, x));
                Ok(format!("Factorizations: {}{}", results, if len < 10 {""} else {";  ..."}))
            },
            Query::Expr(ref expr) => {
                let val = try!(self.eval(expr));
                Ok(val.show(self))
            },
            Query::Error(ref e) => Err(e.clone()),
        }
    }

    /// Creates a new, empty context
    pub fn new() -> Context {
        Context {
            dimensions: Vec::new(),
            units: HashMap::new(),
            aliases: HashMap::new(),
            reverse: HashMap::new(),
            prefixes: Vec::new(),
            definitions: HashMap::new(),
            datepatterns: Vec::new(),
            short_output: false,
        }
    }

    pub fn load_dates(&mut self, mut dates: Vec<Vec<DatePattern>>) {
        self.datepatterns.append(&mut dates)
    }

    /// Takes a parsed definitions.units from
    /// `gnu_units::parse()`. Prints if there are errors in the file.
    pub fn load(&mut self, defs: Defs) {
        use std::collections::HashSet;

        #[derive(PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Clone)]
        enum Name {
            Unit(Rc<String>),
            Prefix(Rc<String>),
            Quantity(Rc<String>),
        }

        struct Resolver {
            interned: HashSet<Rc<String>>,
            input: HashMap<Name, Rc<Def>>,
            sorted: Vec<Name>,
            unmarked: HashSet<Name>,
            temp_marks: HashSet<Name>,
        }

        impl Resolver {
            fn intern(&mut self, name: &String) -> Rc<String> {
                if let Some(v) = self.interned.get(name).cloned() {
                    v
                } else {
                    let v = Rc::new(name.to_owned());
                    self.interned.insert(v.clone());
                    v
                }
            }

            fn lookup(&mut self, name: &Rc<String>) -> Option<()> {
                let unit = Name::Unit(name.clone());
                if self.input.get(&unit).is_some() {
                    self.visit(&unit);
                    return Some(())
                }
                let unit = Name::Prefix(name.clone());
                if self.input.get(&unit).is_some() {
                    self.visit(&unit);
                    return Some(())
                }
                let unit = Name::Quantity(name.clone());
                if self.input.get(&unit).is_some() {
                    self.visit(&unit);
                    return Some(())
                }
                if name.ends_with("s") {
                    let v = Rc::new(name[0..name.len()-1].to_owned());
                    if let Some(()) = self.lookup(&v) {
                        return Some(())
                    }
                }
                let mut found = vec![];
                for (pre, _) in &self.input {
                    if let &Name::Prefix(ref pre) = pre {
                        if (*name).starts_with(&**pre) {
                            found.push(pre.clone());
                        }
                    }
                }
                for pre in found {
                    if let Some(()) = self.lookup(&Rc::new(name[pre.len()..].to_owned())) {
                        let unit = Name::Prefix(pre);
                        self.visit(&unit);
                        return Some(())
                    }
                }
                None
            }

            fn eval(&mut self, expr: &Expr) {
                match *expr {
                    Expr::Unit(ref name) => {
                        let name = self.intern(name);
                        if self.lookup(&name).is_none() {
                            println!("Lookup failed: {}", name);
                        }
                    },
                    Expr::Frac(ref left, ref right) |
                    Expr::Pow(ref left, ref right) |
                    Expr::Add(ref left, ref right) |
                    Expr::Sub(ref left, ref right) => {
                        self.eval(left);
                        self.eval(right);
                    },
                    Expr::Neg(ref expr) | Expr::Plus(ref expr) | Expr::Suffix(_, ref expr) =>
                        self.eval(expr),
                    Expr::Mul(ref exprs) | Expr::Call(_, ref exprs) => for expr in exprs {
                        self.eval(expr);
                    },
                    _ => ()
                }
            }

            fn visit(&mut self, name: &Name) {
                if self.temp_marks.get(name).is_some() {
                    println!("Unit {:?} has a dependency cycle", name);
                    return;
                }
                if self.unmarked.get(name).is_some() {
                    self.temp_marks.insert(name.clone());
                    if let Some(v) = self.input.get(name).cloned() {
                        match *v {
                            Def::Prefix(ref e) | Def::SPrefix(ref e) | Def::Unit(ref e) |
                            Def::Quantity(ref e) =>
                                self.eval(e),
                            _ => (),
                        }
                    }
                    self.unmarked.remove(name);
                    self.temp_marks.remove(name);
                    self.sorted.push(name.clone());
                }
            }
        }

        let mut resolver = Resolver {
            interned: HashSet::new(),
            input: HashMap::new(),
            sorted: vec![],
            unmarked: HashSet::new(),
            temp_marks: HashSet::new(),
        };
        for (name, def) in defs.defs.into_iter() {
            let name = resolver.intern(&name);
            let unit = match *def {
                Def::Prefix(_) | Def::SPrefix(_) => Name::Prefix(name),
                Def::Quantity(_) => Name::Quantity(name),
                _ => Name::Unit(name)
            };
            resolver.input.insert(unit.clone(), def);
            resolver.unmarked.insert(unit);
        }

        while let Some(name) = resolver.unmarked.iter().next().cloned() {
            resolver.visit(&name)
        }
        let sorted = resolver.sorted;
        //println!("{:#?}", sorted);
        let mut input = resolver.input;
        let udefs = sorted.into_iter().map(move |name| {
            let res = input.remove(&name).unwrap();
            (name, res)
        });

        let mut reverse = HashSet::new();
        reverse.insert("newton");
        reverse.insert("pascal");
        reverse.insert("joule");
        reverse.insert("watt");
        reverse.insert("coulomb");
        reverse.insert("volt");
        reverse.insert("ohm");
        reverse.insert("siemens");
        reverse.insert("farad");
        reverse.insert("weber");
        reverse.insert("henry");
        reverse.insert("tesla");

        for (name, def) in udefs {
            let name = match name {
                Name::Unit(name) => (*name).clone(),
                Name::Prefix(name) => (*name).clone(),
                Name::Quantity(name) => (*name).clone(),
            };
            match *def {
                Def::Dimension(ref dname) => {
                    let dname = Rc::new(dname.clone());
                    self.dimensions.push(dname.clone());
                },
                Def::Unit(ref expr) => match self.eval(expr) {
                    Ok(Value::Number(v)) => {
                        if v.0 == Mpq::one() && reverse.contains(&*name) {
                            self.reverse.insert(v.1.clone(), name.clone());
                        }
                        self.definitions.insert(name.clone(), expr.clone());
                        self.units.insert(name.clone(), v);
                    },
                    Ok(_) => println!("Unit {} is not a number", name),
                    Err(e) => println!("Unit {} is malformed: {}", name, e)
                },
                Def::Prefix(ref expr) => match self.eval(expr) {
                    Ok(Value::Number(v)) => {
                        self.prefixes.push((name.clone(), v));
                    },
                    Ok(_) => println!("Prefix {} is not a number", name),
                    Err(e) => println!("Prefix {} is malformed: {}", name, e)
                },
                Def::SPrefix(ref expr) => match self.eval(expr) {
                    Ok(Value::Number(v)) => {
                        self.prefixes.push((name.clone(), v.clone()));
                        self.units.insert(name.clone(), v);
                    },
                    Ok(_) => println!("Prefix {} is not a number", name),
                    Err(e) => println!("Prefix {} is malformed: {}", name, e)
                },
                Def::Quantity(ref expr) => match self.eval(expr) {
                    Ok(Value::Number(v)) => {
                        let res = self.aliases.insert(v.1, name.clone());
                        if !self.definitions.contains_key(&name) {
                            self.definitions.insert(name.clone(), expr.clone());
                        }
                        if let Some(old) = res {
                            println!("Warning: Conflicting quantities {} and {}", name, old);
                        }
                    },
                    Ok(_) => println!("Quantity {} is not a number", name),
                    Err(e) => println!("Quantity {} is malformed: {}", name, e)
                },
                Def::Error(ref err) => println!("Def {}: {}", name, err),
            };
        }
    }
}
