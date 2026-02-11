#![allow(unused_variables)]
#![allow(unused_assignments)]

use crate::signal::ValuesMap;
use crate::{IndexId, OpenPosInfo, Price, TimeDelta, TimeFrame, timedelta};

const MARKET_ORDER_TIMEOUT: TimeDelta = timedelta!(TimeFrame::Min1, 1);

#[derive(Debug, Clone)]
pub struct StratContext<'a> {
    pub free_margin: f64,
    pub last_price: Price,
    pub indicators: &'a ValuesMap,
}

pub trait Strat: Send {
    fn on_idle(&mut self, ctx: StratContext, is_armed: Armed) -> Option<Intent>;
    fn on_busy(&mut self, ctx: StratContext, busy_reason: BusyType) -> Option<Intent>;
    fn on_open(&mut self, ctx: StratContext, open_pos: &OpenPosInfo) -> Option<Intent>;
    fn required_indicators(&self) -> Vec<IndexId>;
}

pub trait NeedsIndicators {
    fn required_indicators_static() -> Vec<IndexId>;
}

pub type Armed = Option<u64>; //expiry time

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct LiveTimeoutInfo {
    pub expire_at: u64,
    pub timeout_info: TimeoutInfo,
    pub intent: Intent,
}

impl LiveTimeoutInfo {
    pub fn expires_in(&self) -> TimeDelta {
        self.timeout_info.duration
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum BusyType {
    Opening(Option<LiveTimeoutInfo>),
    Closing(Option<LiveTimeoutInfo>),
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum SizeSpec {
    MarginAmount(f64),
    MarginPct(f64), // % of free margin OR % of open pos used_margin
    RawSize(f64),   // number of asset units
}

impl SizeSpec {
    pub(crate) fn get_size(&self, free_margin: f64, ref_px: f64) -> f64 {
        match self {
            SizeSpec::RawSize(sz) => *sz,
            SizeSpec::MarginAmount(amount) => amount / ref_px,

            SizeSpec::MarginPct(pct) => {
                let amount = free_margin * (pct / 100.0);
                amount / ref_px
            }
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OnTimeout {
    Force,
    Cancel,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct TimeoutInfo {
    pub action: OnTimeout,
    pub duration: TimeDelta,
}

impl Default for TimeoutInfo {
    fn default() -> Self {
        TimeoutInfo {
            action: OnTimeout::Cancel,
            duration: MARKET_ORDER_TIMEOUT,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Order {
    pub size: SizeSpec,
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum Intent {
    Open(Order),
    Close(Order),
    Flatten,
    Arm(TimeDelta), //timeout duration
    Disarm,
    Abort, //Force close at market
}

impl Intent {
    pub fn open_market(size: SizeSpec) -> Self {
        Intent::Open(Order { size })
    }

    pub fn close_market(size: SizeSpec) -> Self {
        Intent::Close(Order { size })
    }

    pub fn flatten_market() -> Self {
        Intent::Flatten
    }
}

impl Intent {
    pub fn get_ttl(&self) -> Option<TimeoutInfo> {
        None
    }

    pub fn is_order(&self) -> bool {
        matches!(self, Intent::Open(_) | Intent::Close(_) | Intent::Flatten)
    }

    pub fn is_market_order(&self) -> bool {
        matches!(
            self,
            Intent::Open(_) | Intent::Close(_) | Intent::Flatten | Intent::Abort
        )
    }

    pub fn is_limit_order(&self) -> bool {
        !self.is_market_order()
    }
}
