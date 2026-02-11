#![allow(unused_variables)]
use super::*;
use TimeFrame::*;
use Value::*;

const RSI_LONG_MIN: f64 = 40.0;
const RSI_LONG_MAX: f64 = 50.0;
const ENTRY_MARGIN_PCT: f64 = 95.0;

pub struct ElderTripleScreen {
    rsi_14_h1: IndexId,
}

impl ElderTripleScreen {
    pub fn init() -> Self {
        let inds = Self::required_indicators_static();
        Self { rsi_14_h1: inds[0] }
    }
}

impl NeedsIndicators for ElderTripleScreen {
    fn required_indicators_static() -> Vec<IndexId> {
        vec![(IndicatorKind::Rsi(14), Hour1)]
    }
}

impl Strat for ElderTripleScreen {
    fn required_indicators(&self) -> Vec<IndexId> {
        Self::required_indicators_static()
    }

    fn on_idle(&mut self, ctx: StratContext, _armed: Armed) -> Option<Intent> {
        let rsi_1h = match ctx.indicators.get(&self.rsi_14_h1)?.value {
            RsiValue(v) => v,
            _ => return None,
        };

        if rsi_1h >= RSI_LONG_MIN && rsi_1h <= RSI_LONG_MAX {
            return Some(Intent::open_market(SizeSpec::MarginPct(ENTRY_MARGIN_PCT)));
        }

        None
    }

    fn on_open(&mut self, ctx: StratContext, _open_pos: &OpenPosInfo) -> Option<Intent> {
        let rsi_1h = match ctx.indicators.get(&self.rsi_14_h1)?.value {
            RsiValue(v) => v,
            _ => return None,
        };

        if rsi_1h > RSI_LONG_MAX {
            return Some(Intent::flatten_market());
        }

        None
    }

    fn on_busy(&mut self, _ctx: StratContext, _busy: BusyType) -> Option<Intent> {
        None
    }
}
