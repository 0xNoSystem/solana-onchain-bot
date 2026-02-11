#![allow(unused_variables)]
use super::*;
use TimeFrame::*;
use Value::*;

const RSI_THRESH: f64 = 50.0;
const ADX_THRESH: f64 = 35.0;
const NATR_THRESH: f64 = 0.02;

pub struct RsiChopSwing {
    rsi_1h: IndexId,
    adx_12h: IndexId,
    atr_1d: IndexId,
}

impl RsiChopSwing {
    pub fn init() -> Self {
        let inds = Self::required_indicators_static();
        Self {
            rsi_1h: inds[0],
            adx_12h: inds[1],
            atr_1d: inds[2],
        }
    }
}

impl NeedsIndicators for RsiChopSwing {
    fn required_indicators_static() -> Vec<IndexId> {
        vec![
            (IndicatorKind::Rsi(12), Hour1),
            (IndicatorKind::Adx { periods: 14, di_length: 10 }, Hour12),
            (IndicatorKind::Atr(14), Day1),
        ]
    }
}

impl Strat for RsiChopSwing {
    fn required_indicators(&self) -> Vec<IndexId> {
        Self::required_indicators_static()
    }

    fn on_idle(&mut self, ctx: StratContext, armed: Armed) -> Option<Intent> {
        let StratContext {
            free_margin,
            last_price,
            indicators,
        } = ctx;

        let px = last_price.close;

        let max_size = free_margin / px;

        let rsi_1h_value = match indicators.get(&self.rsi_1h)?.value {
            RsiValue(v) => v,
            _ => return None,
        };

        let atr_1d_value = match indicators.get(&self.atr_1d)?.value {
            AtrValue(v) => v,
            _ => return None,
        };

        let adx_12h_value = match indicators.get(&self.adx_12h)?.value {
            AdxValue(v) => v,
            _ => return None,
        };

        let atr_normalized = atr_1d_value / px;

        if armed.is_none() && (atr_normalized > NATR_THRESH && adx_12h_value < ADX_THRESH) {
            return Some(Intent::Arm(timedelta!(Hour1, 10)));
        }

        if armed.is_some() {
            let size = max_size * 0.9;

            if rsi_1h_value < RSI_THRESH {
                return Some(Intent::open_market(SizeSpec::RawSize(size)));
            }
        }

        None
    }

    fn on_open(&mut self, ctx: StratContext, open_pos: &OpenPosInfo) -> Option<Intent> {
        let StratContext {
            free_margin,
            last_price,
            indicators,
        } = ctx;

        let px = last_price.close;

        let rsi_1h_value = match indicators.get(&self.rsi_1h)?.value {
            RsiValue(v) => v,
            _ => return None,
        };

        if rsi_1h_value > 52.0 {
            return Some(Intent::flatten_market());
        }

        None
    }

    fn on_busy(&mut self, ctx: StratContext, busy_reason: BusyType) -> Option<Intent> {
        None
    }
}
