
pub const ID_EMPTY: u64 = u64::MAX;
pub const ID_ODD: u64 = 1 << 63;
pub const TS_BAD: i64 = i64::MIN;
pub const TS_HUGE: i64 = i64::MAX;
pub const PROD_EMPTY: u32 = u32::MAX;
pub const PROD_ODD: u32 = u32::MAX - 1;
pub const CAMP_EMPTY: u32 = u32::MAX;
pub const CAMP_ODD: u32 = u32::MAX - 1;
pub const KIND_VIEW: u8 = 0;
pub const KIND_CART: u8 = 1;
pub const KIND_PURCHASE: u8 = 2;
pub const KIND_OTHER: u8 = 3;

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct EvRow {
    pub eid: u64,
    pub uid: u64,
    pub ts: i64,
    pub ingest: i64,
    pub kind: u8,
    pub product: u32,
    pub price: u32,
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ClRow {
    pub cid: u64,
    pub uid: u64,
    pub campaign: u32,
    pub ts: i64,
}

pub trait EventSink {
    fn event(&mut self, r: &EvRow);
}
pub trait ClickSink {
    fn click(&mut self, r: &ClRow);
}

pub trait Kernel {
    const NAME: &'static str;
    fn events<S: EventSink>(buf: &[u8], sink: &mut S);
    fn clicks<S: ClickSink>(buf: &[u8], sink: &mut S);
}

#[inline(always)]
pub fn odd_id(offset: usize, len: usize) -> u64 {
    ID_ODD | ((offset as u64) << 8) | (len.min(255) as u64)
}
#[inline(always)]
pub fn id_is_odd(id: u64) -> bool {
    id != ID_EMPTY && id & ID_ODD != 0
}
#[inline(always)]
pub fn odd_offset(id: u64) -> usize {
    ((id & !ID_ODD) >> 8) as usize
}
#[inline(always)]
pub fn odd_len(id: u64) -> usize {
    (id & 255) as usize
}
