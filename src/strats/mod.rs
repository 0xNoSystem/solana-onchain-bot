#![allow(clippy::all)]
pub(super) use crate::{
    Armed, BusyType, IndexId, IndicatorKind, Intent, NeedsIndicators, OpenPosInfo, SizeSpec, Strat,
    StratContext, TimeFrame, Value, timedelta,
};

include!(concat!(env!("OUT_DIR"), "/strats_gen.rs"));
