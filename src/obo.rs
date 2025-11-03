// Normalized L3 (Order-by-Order) events and mapping from internal parser events

use crate::codec_raw::{OboAddV1, OboCancelV1, OboExecuteV1, OboModifyV1};
use crate::parser::{Event, Side};

#[derive(Debug, Clone, Copy)]
pub enum OboEventV1 {
    Add(OboAddV1),
    Modify(OboModifyV1),
    Cancel(OboCancelV1),
    Execute(OboExecuteV1),
}

#[inline]
fn side_to_u8(side: Side) -> u8 {
    match side {
        Side::Bid => 0,
        Side::Ask => 1,
    }
}

#[inline]
pub fn map_event_to_obo_parts(ev: &Event) -> (Option<u32>, Option<OboEventV1>) {
    match *ev {
        Event::Add {
            order_id,
            instr,
            px,
            qty,
            side,
        } => {
            (
                Some(instr),
                Some(OboEventV1::Add(OboAddV1 {
                    order_id,
                    price_e8: px, // assume upstream px already scaled; revisit if needed
                    qty: qty as u64,
                    side: side_to_u8(side),
                    flags: 0,
                })),
            )
        }
        Event::Mod { order_id, qty } => {
            // qty-only modify; leave price unchanged (encode as 0 with a flag)
            (
                None,
                Some(OboEventV1::Modify(OboModifyV1 {
                    order_id,
                    new_price_e8: 0,
                    new_qty: qty as u64,
                    flags: 1, // 1 = qty-only
                })),
            )
        }
        Event::Del { order_id } => (
            None,
            Some(OboEventV1::Cancel(OboCancelV1 {
                order_id,
                qty_cxl: 0,
                reason: 0,
            })),
        ),
        Event::Trade {
            instr,
            px,
            qty,
            maker_order_id,
            taker_side,
        } => {
            if let Some(maker) = maker_order_id {
                let side = taker_side.map(side_to_u8).unwrap_or(0);
                (
                    Some(instr),
                    Some(OboEventV1::Execute(OboExecuteV1 {
                        maker_order_id: maker,
                        trade_qty: qty as u64,
                        trade_price_e8: px,
                        aggressor_side: side,
                        match_id: 0,
                    })),
                )
            } else {
                (Some(instr), None)
            }
        }
        Event::Heartbeat => (None, None),
    }
}
