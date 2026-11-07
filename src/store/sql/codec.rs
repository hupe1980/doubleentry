//! How values are stored: amounts as 16-byte big-endian
//! two's complement, dates as Julian day numbers, small enums as fixed
//! codes, dimension sets as text — and the strict decoding of each, where
//! every out-of-domain value is an integrity failure, never a default.

use time::Date;

use super::dialect::{Kind, Value};
use crate::core::{BalanceKey, ResolutionKind, Window};
use crate::error::IntegrityKind;
use crate::log::Hash;
use crate::model::{
    AccountHandle, AccountKind, AccountPath, Amount, Balance, BalanceLimit, Currency, Dimensions,
    EntryId, Fingerprint, IdempotencyKey, Label, Layer, MemoSalt, PeriodId, RecordKind, Side,
};
use crate::store::StoreError;

/// Separates a dimension's axis from its value (labels hold no control
/// characters).
const AXIS_VALUE: char = '\u{1f}';
/// Separates the pairs of a dimension set.
const PAIRS: char = '\u{1e}';

/// A count or position as an integer parameter; `Backend` above
/// `i64::MAX`.
pub(crate) fn int(value: u64) -> Result<Value, StoreError> {
    i64::try_from(value)
        .map(Value::Int)
        .map_err(|_| StoreError::backend_message("encode", "an integer above i64::MAX"))
}

/// A date as its Julian day number.
pub(crate) fn date(date: Date) -> Value {
    Value::Int(i64::from(date.to_julian_day()))
}

/// An optional date.
pub(crate) fn opt_date(date: Option<Date>) -> Value {
    date.map_or(Value::Null(Kind::Int), self::date)
}

/// An amount as 16 bytes, big-endian two's complement.
pub(crate) fn amount<const P: u8>(amount: Amount<P>) -> Value {
    Value::Bytes(amount.to_minor().to_be_bytes().to_vec())
}

/// Text.
pub(crate) fn text(text: &str) -> Value {
    Value::Text(text.to_owned())
}

/// Bytes.
pub(crate) fn bytes(bytes: &[u8]) -> Value {
    Value::Bytes(bytes.to_vec())
}

/// An account handle.
pub(crate) fn handle(handle: AccountHandle) -> Value {
    Value::Int(i64::from(handle.get()))
}

/// How a hold was resolved, as its code.
pub(crate) fn how(how: ResolutionKind) -> Value {
    Value::Int(match how {
        ResolutionKind::Posted => 0,
        ResolutionKind::Voided => 1,
        ResolutionKind::Expired => 2,
    })
}

/// The three columns of a balance key.
pub(crate) fn key(key: BalanceKey) -> [Value; 3] {
    [
        handle(key.handle),
        text(key.currency.as_str()),
        layer(key.layer),
    ]
}

/// A layer's code.
pub(crate) const fn layer(layer: Layer) -> Value {
    Value::Int(match layer {
        Layer::Settled => 0,
        Layer::Pending => 1,
    })
}

/// A side's code.
pub(crate) const fn side(side: Side) -> Value {
    Value::Int(match side {
        Side::Debit => 0,
        Side::Credit => 1,
    })
}

/// An account kind's code.
pub(crate) const fn kind(kind: AccountKind) -> Value {
    Value::Int(match kind {
        AccountKind::Asset => 0,
        AccountKind::Liability => 1,
        AccountKind::Equity => 2,
        AccountKind::Income => 3,
        AccountKind::Expense => 4,
    })
}

/// A balance limit's code.
pub(crate) const fn limit(limit: BalanceLimit) -> Value {
    Value::Int(match limit {
        BalanceLimit::Unlimited => 0,
        BalanceLimit::NoCreditBalance => 1,
        BalanceLimit::NoDebitBalance => 2,
    })
}

/// A dimension set as text: `axis US value` pairs joined by `RS`, in the
/// set's (byte) order.
pub(crate) fn dimensions(dimensions: &Dimensions) -> Value {
    let mut encoded = String::new();
    for (axis, value) in dimensions.iter() {
        if !encoded.is_empty() {
            encoded.push(PAIRS);
        }
        encoded.push_str(axis.as_str());
        encoded.push(AXIS_VALUE);
        encoded.push_str(value.as_str());
    }
    Value::Text(encoded)
}

/// A window's columns (open from, open to).
pub(crate) fn window(window: Window) -> [Value; 2] {
    [date(window.open_from), opt_date(window.open_to)]
}

/// One returned row, decoded strictly.
pub(crate) struct Row {
    operation: &'static str,
    values: Vec<Value>,
}

impl Row {
    /// A row of `values` returned to `operation`.
    pub(crate) const fn new(operation: &'static str, values: Vec<Value>) -> Self {
        Self { operation, values }
    }

    fn bad(&self, what: &'static str) -> StoreError {
        StoreError::integrity(self.operation, what, None)
    }

    fn value(&self, index: usize) -> Result<&Value, StoreError> {
        self.values
            .get(index)
            .ok_or_else(|| self.bad("a row has too few columns"))
    }

    /// Whether column `index` is `NULL`.
    pub(crate) fn is_null(&self, index: usize) -> Result<bool, StoreError> {
        Ok(matches!(self.value(index)?, Value::Null(_)))
    }

    /// An integer column.
    pub(crate) fn int(&self, index: usize) -> Result<i64, StoreError> {
        match self.value(index)? {
            Value::Int(value) => Ok(*value),
            _ => Err(self.bad("an integer column holds something else")),
        }
    }

    /// A non-negative integer column.
    pub(crate) fn u64(&self, index: usize) -> Result<u64, StoreError> {
        u64::try_from(self.int(index)?).map_err(|_| self.bad("a count or position is negative"))
    }

    /// A nullable non-negative integer column.
    pub(crate) fn opt_u64(&self, index: usize) -> Result<Option<u64>, StoreError> {
        if self.is_null(index)? {
            Ok(None)
        } else {
            self.u64(index).map(Some)
        }
    }

    /// A posting index column.
    pub(crate) fn u16(&self, index: usize) -> Result<u16, StoreError> {
        u16::try_from(self.int(index)?).map_err(|_| self.bad("a posting index is out of range"))
    }

    /// A small code column.
    pub(crate) fn u8(&self, index: usize) -> Result<u8, StoreError> {
        u8::try_from(self.int(index)?).map_err(|_| self.bad("a code is out of range"))
    }

    /// A byte column.
    pub(crate) fn bytes(&self, index: usize) -> Result<&[u8], StoreError> {
        match self.value(index)? {
            Value::Bytes(bytes) => Ok(bytes),
            _ => Err(self.bad("a byte column holds something else")),
        }
    }

    /// A byte column, taken out of the row.
    pub(crate) fn take_bytes(&mut self, index: usize) -> Result<Vec<u8>, StoreError> {
        let taken = match self.values.get_mut(index) {
            Some(Value::Bytes(bytes)) => Some(core::mem::take(bytes)),
            _ => None,
        };
        taken.ok_or_else(|| self.bad("a byte column holds something else"))
    }

    /// A nullable byte column.
    pub(crate) fn opt_bytes(&self, index: usize) -> Result<Option<&[u8]>, StoreError> {
        if self.is_null(index)? {
            Ok(None)
        } else {
            self.bytes(index).map(Some)
        }
    }

    /// A text column.
    pub(crate) fn text(&self, index: usize) -> Result<&str, StoreError> {
        match self.value(index)? {
            Value::Text(text) => Ok(text),
            _ => Err(self.bad("a text column holds something else")),
        }
    }

    /// A nullable text column.
    pub(crate) fn opt_text(&self, index: usize) -> Result<Option<&str>, StoreError> {
        if self.is_null(index)? {
            Ok(None)
        } else {
            self.text(index).map(Some)
        }
    }

    /// A fixed-length byte column.
    fn array<const N: usize>(
        &self,
        index: usize,
        what: &'static str,
    ) -> Result<[u8; N], StoreError> {
        self.bytes(index)?.try_into().map_err(|_| self.bad(what))
    }

    /// A date column.
    pub(crate) fn date(&self, index: usize) -> Result<Date, StoreError> {
        let day =
            i32::try_from(self.int(index)?).map_err(|_| self.bad("a date is out of range"))?;
        Date::from_julian_day(day).map_err(|_| self.bad("a date is out of range"))
    }

    /// A nullable date column.
    pub(crate) fn opt_date(&self, index: usize) -> Result<Option<Date>, StoreError> {
        if self.is_null(index)? {
            Ok(None)
        } else {
            self.date(index).map(Some)
        }
    }

    /// An amount column.
    pub(crate) fn amount<const P: u8>(&self, index: usize) -> Result<Amount<P>, StoreError> {
        let minor = i128::from_be_bytes(self.array(index, "an amount is not 16 bytes")?);
        Amount::from_minor(minor).map_err(|_| self.bad("an amount is out of range"))
    }

    /// A balance from three columns (debit, credit, postings).
    pub(crate) fn balance<const P: u8>(&self, index: usize) -> Result<Balance<P>, StoreError> {
        let credit_at = index.saturating_add(1);
        let postings_at = index.saturating_add(2);
        Balance::from_parts(
            self.amount(index)?,
            self.amount(credit_at)?,
            self.u64(postings_at)?,
        )
        .map_err(|_| self.bad("a balance is out of range"))
    }

    /// An account handle column.
    pub(crate) fn handle(&self, index: usize) -> Result<AccountHandle, StoreError> {
        u32::try_from(self.int(index)?)
            .ok()
            .and_then(AccountHandle::new)
            .ok_or_else(|| self.bad("an account handle is out of range"))
    }

    /// A currency column.
    pub(crate) fn currency(&self, index: usize) -> Result<Currency, StoreError> {
        Currency::new(self.text(index)?).map_err(|_| self.bad("a currency code is invalid"))
    }

    /// A layer column.
    pub(crate) fn layer(&self, index: usize) -> Result<Layer, StoreError> {
        match self.int(index)? {
            0 => Ok(Layer::Settled),
            1 => Ok(Layer::Pending),
            _ => Err(self.bad("a layer code is invalid")),
        }
    }

    /// A side column.
    pub(crate) fn side(&self, index: usize) -> Result<Side, StoreError> {
        match self.int(index)? {
            0 => Ok(Side::Debit),
            1 => Ok(Side::Credit),
            _ => Err(self.bad("a side code is invalid")),
        }
    }

    /// An account kind column.
    pub(crate) fn kind(&self, index: usize) -> Result<AccountKind, StoreError> {
        match self.int(index)? {
            0 => Ok(AccountKind::Asset),
            1 => Ok(AccountKind::Liability),
            2 => Ok(AccountKind::Equity),
            3 => Ok(AccountKind::Income),
            4 => Ok(AccountKind::Expense),
            _ => Err(self.bad("an account kind code is invalid")),
        }
    }

    /// A balance limit column.
    pub(crate) fn limit(&self, index: usize) -> Result<BalanceLimit, StoreError> {
        match self.int(index)? {
            0 => Ok(BalanceLimit::Unlimited),
            1 => Ok(BalanceLimit::NoCreditBalance),
            2 => Ok(BalanceLimit::NoDebitBalance),
            _ => Err(self.bad("a balance limit code is invalid")),
        }
    }

    /// A record kind column.
    pub(crate) fn record_kind(&self, index: usize) -> Result<RecordKind, StoreError> {
        RecordKind::from_u8(self.u8(index)?).ok_or_else(|| self.bad("a record kind is invalid"))
    }

    /// A balance key from three columns (handle, currency, layer).
    pub(crate) fn key(&self, index: usize) -> Result<BalanceKey, StoreError> {
        Ok(BalanceKey {
            handle: self.handle(index)?,
            currency: self.currency(index.saturating_add(1))?,
            layer: self.layer(index.saturating_add(2))?,
        })
    }

    /// An entry id column.
    pub(crate) fn entry_id(&self, index: usize) -> Result<EntryId, StoreError> {
        Ok(EntryId::from_bytes(
            self.array(index, "an entry id is not 16 bytes")?,
        ))
    }

    /// A hold's resolution from two columns (resolving entry, how); `None`
    /// while the first is null.
    pub(crate) fn resolution(
        &self,
        index: usize,
    ) -> Result<Option<(EntryId, ResolutionKind)>, StoreError> {
        if self.is_null(index)? {
            return Ok(None);
        }
        let how = match self.int(index.saturating_add(1))? {
            0 => ResolutionKind::Posted,
            1 => ResolutionKind::Voided,
            2 => ResolutionKind::Expired,
            _ => return Err(self.bad("a resolution code is invalid")),
        };
        Ok(Some((self.entry_id(index)?, how)))
    }

    /// A hash column.
    pub(crate) fn hash(&self, index: usize) -> Result<Hash, StoreError> {
        Ok(Hash::from_bytes(
            self.array(index, "a hash is not 32 bytes")?,
        ))
    }

    /// A nullable fingerprint column.
    pub(crate) fn fingerprint(&self, index: usize) -> Result<Option<Fingerprint>, StoreError> {
        if self.is_null(index)? {
            return Ok(None);
        }
        Ok(Some(Fingerprint::from_bytes(
            self.array(index, "a fingerprint is not 32 bytes")?,
        )))
    }

    /// A memo salt column.
    pub(crate) fn salt(&self, index: usize) -> Result<MemoSalt, StoreError> {
        Ok(MemoSalt(self.array(index, "a memo salt is not 16 bytes")?))
    }

    /// An idempotency key column.
    pub(crate) fn idempotency_key(&self, index: usize) -> Result<IdempotencyKey, StoreError> {
        let text = core::str::from_utf8(self.bytes(index)?)
            .map_err(|_| self.bad("an idempotency key is not UTF-8"))?;
        IdempotencyKey::new(text).map_err(|_| self.bad("an idempotency key is invalid"))
    }

    /// An account path column.
    pub(crate) fn path(&self, index: usize) -> Result<AccountPath, StoreError> {
        AccountPath::parse(self.text(index)?).map_err(|_| self.bad("an account path is invalid"))
    }

    /// A label column.
    pub(crate) fn label(&self, index: usize) -> Result<Label, StoreError> {
        Label::new(self.text(index)?).map_err(|_| self.bad("a label is invalid"))
    }

    /// A nullable label column.
    pub(crate) fn opt_label(&self, index: usize) -> Result<Option<Label>, StoreError> {
        self.opt_text(index)?
            .map(|text| Label::new(text).map_err(|_| self.bad("a label is invalid")))
            .transpose()
    }

    /// A period id column.
    pub(crate) fn period(&self, index: usize) -> Result<PeriodId, StoreError> {
        self.label(index)
    }

    /// A dimension set column.
    pub(crate) fn dimensions(&self, index: usize) -> Result<Dimensions, StoreError> {
        let text = self.text(index)?;
        if text.is_empty() {
            return Ok(Dimensions::new());
        }
        text.split(PAIRS).try_fold(Dimensions::new(), |set, pair| {
            let (axis, value) = pair
                .split_once(AXIS_VALUE)
                .ok_or_else(|| self.bad("a dimension set is malformed"))?;
            let axis = Label::new(axis).map_err(|_| self.bad("a dimension axis is invalid"))?;
            let value = Label::new(value).map_err(|_| self.bad("a dimension value is invalid"))?;
            set.with(axis, value)
                .map_err(|_| self.bad("a dimension set is malformed"))
        })
    }
}

/// A stored count or position that does not fit the type the core uses.
pub(crate) fn out_of_range(operation: &'static str) -> StoreError {
    StoreError::integrity_of(
        operation,
        IntegrityKind::Undecodable,
        "a stored count is out of range",
        None,
    )
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn row(values: Vec<Value>) -> Row {
        Row::new("test", values)
    }

    proptest! {
        #[test]
        fn amounts_round_trip(minor in -i128::MAX..=i128::MAX) {
            let amount = Amount::<2>::from_minor(minor).unwrap();
            prop_assert_eq!(row(vec![super::amount(amount)]).amount::<2>(0).unwrap(), amount);
        }

        #[test]
        fn dates_round_trip(day in Date::MIN.to_julian_day()..=Date::MAX.to_julian_day()) {
            let day = Date::from_julian_day(day).unwrap();
            prop_assert_eq!(row(vec![date(day)]).date(0).unwrap(), day);
        }
    }

    #[test]
    fn extremes_round_trip() {
        for minor in [i128::MAX, -i128::MAX, 0, 1, -1] {
            let amount = Amount::<2>::from_minor(minor).unwrap();
            assert_eq!(
                row(vec![super::amount(amount)]).amount::<2>(0).unwrap(),
                amount
            );
        }
        for day in [Date::MIN, Date::MAX] {
            assert_eq!(row(vec![date(day)]).date(0).unwrap(), day);
        }
        for side in [Side::Debit, Side::Credit] {
            assert_eq!(row(vec![super::side(side)]).side(0).unwrap(), side);
        }
        for layer in [Layer::Settled, Layer::Pending] {
            assert_eq!(row(vec![super::layer(layer)]).layer(0).unwrap(), layer);
        }
        for limit in [
            BalanceLimit::Unlimited,
            BalanceLimit::NoCreditBalance,
            BalanceLimit::NoDebitBalance,
        ] {
            assert_eq!(row(vec![super::limit(limit)]).limit(0).unwrap(), limit);
        }
        for kind in [
            AccountKind::Asset,
            AccountKind::Liability,
            AccountKind::Equity,
            AccountKind::Income,
            AccountKind::Expense,
        ] {
            assert_eq!(row(vec![super::kind(kind)]).kind(0).unwrap(), kind);
        }
    }

    #[test]
    fn out_of_domain_values_are_integrity_failures() {
        let refused = |result: Result<(), StoreError>| {
            assert_eq!(
                result.unwrap_err().kind(),
                &crate::store::StoreErrorKind::Integrity
            );
        };
        refused(
            row(vec![Value::Bytes(vec![0; 15])])
                .amount::<2>(0)
                .map(drop),
        );
        refused(
            row(vec![Value::Bytes(i128::MIN.to_be_bytes().to_vec())])
                .amount::<2>(0)
                .map(drop),
        );
        refused(row(vec![Value::Int(7)]).side(0).map(drop));
        refused(row(vec![Value::Int(2)]).layer(0).map(drop));
        refused(row(vec![Value::Int(5)]).kind(0).map(drop));
        refused(row(vec![Value::Int(i64::MAX)]).date(0).map(drop));
        refused(row(vec![Value::Text(String::from("x"))]).int(0).map(drop));
        refused(row(vec![Value::Int(-1)]).u64(0).map(drop));
        refused(
            row(vec![Value::Text(String::from("a"))])
                .dimensions(0)
                .map(drop),
        );
    }

    #[test]
    fn dimension_sets_round_trip_in_byte_order() {
        let set = Dimensions::new()
            .with(Label::new("region").unwrap(), Label::new("émigré").unwrap())
            .unwrap()
            .with(Label::new("a=b").unwrap(), Label::new("Zebra").unwrap())
            .unwrap();
        let Value::Text(text) = dimensions(&set) else {
            panic!("dimension sets are text");
        };
        assert!(text.starts_with("a=b\u{1f}Zebra\u{1e}region"));
        assert_eq!(row(vec![Value::Text(text)]).dimensions(0).unwrap(), set);
        assert_eq!(
            row(vec![dimensions(&Dimensions::new())])
                .dimensions(0)
                .unwrap(),
            Dimensions::new()
        );
    }
}
