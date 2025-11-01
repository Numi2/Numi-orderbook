// src/orderbook.rs Numan Thabit: extended with export/import
use crate::parser::{Event, Side};
use hashbrown::HashMap;
use serde::{Deserialize, Serialize};
use slab::Slab;
use smallvec::SmallVec;
use std::collections::BTreeMap;

type Handle = usize;

#[derive(Clone, Debug)]
struct Node {
    order_id: u64,
    price: i64,
    qty: i64,
    side: Side,
    prev: Option<Handle>,
    next: Option<Handle>,
}

impl Node {
    #[inline] fn new(order_id: u64, price: i64, qty: i64, side: Side) -> Self {
        Self { order_id, price, qty, side, prev: None, next: None }
    }
}

#[derive(Clone, Debug, Default)]
struct Level {
    head: Option<Handle>,
    tail: Option<Handle>,
    total_qty: i64,
    count: usize,
}

impl Level {
    #[inline] fn is_empty(&self) -> bool { self.count == 0 }

    // Methods operating purely on Level are kept minimal; order-node mutation is handled in InstrumentBook

    /// Iterate handles FIFO from head to tail
    fn iter_fifo<'a>(&self, orders: &'a Slab<Node>) -> LevelIter<'a> {
        LevelIter { orders, cur: self.head }
    }
}

struct LevelIter<'a> {
    orders: &'a Slab<Node>,
    cur: Option<Handle>,
}
impl<'a> Iterator for LevelIter<'a> {
    type Item = Handle;
    fn next(&mut self) -> Option<Self::Item> {
        if let Some(h) = self.cur {
            self.cur = self.orders[h].next;
            Some(h)
        } else { None }
    }
}

#[derive(Default)]
struct InstrumentBook {
    bids: BTreeMap<i64, Level>,
    asks: BTreeMap<i64, Level>,
    orders: Slab<Node>,
}

impl InstrumentBook {
    #[cfg(test)]
    fn new() -> Self { Self { bids: BTreeMap::new(), asks: BTreeMap::new(), orders: Slab::with_capacity(1<<20) } }

    #[inline]
    fn levels_mut(&mut self, side: Side) -> &mut BTreeMap<i64, Level> {
        match side { Side::Bid => &mut self.bids, Side::Ask => &mut self.asks }
    }

    fn add(&mut self, order_id: u64, price: i64, qty: i64, side: Side) -> Handle {
        let h = self.orders.insert(Node::new(order_id, price, qty, side));
        // Obtain previous tail without holding the level borrow across order mutations
        let prev_tail = {
            let lvl = self.levels_mut(side).entry(price).or_default();
            lvl.tail
        };
        if let Some(t) = prev_tail { self.orders[t].next = Some(h); }
        {
            let n = &mut self.orders[h];
            n.prev = prev_tail;
            n.next = None;
        }
        {
            let lvl = self.levels_mut(side).entry(price).or_default();
            if prev_tail.is_none() { lvl.head = Some(h); }
            lvl.tail = Some(h);
            lvl.count += 1;
            lvl.total_qty += qty;
        }
        h
    }

    fn set_qty(&mut self, h: Handle, new_qty: i64) {
        let (price, side, old_qty) = {
            let n = &self.orders[h];
            (n.price, n.side, n.qty)
        };
        {
            let n = &mut self.orders[h];
            n.qty = new_qty;
        }
        if let Some(lvl) = self.levels_mut(side).get_mut(&price) {
            lvl.total_qty += new_qty - old_qty;
        }
    }

    fn cancel(&mut self, h: Handle) {
        let (price, side, prev, next, qty) = {
            let n = &self.orders[h];
            (n.price, n.side, n.prev, n.next, n.qty)
        };
        if let Some(p) = prev { self.orders[p].next = next; }
        if let Some(nh) = next { self.orders[nh].prev = prev; }
        if let Some(lvl) = self.levels_mut(side).get_mut(&price) {
            if prev.is_none() { lvl.head = next; }
            if next.is_none() { lvl.tail = prev; }
            lvl.count = lvl.count.saturating_sub(1);
            lvl.total_qty -= qty;
            if lvl.is_empty() {
                self.levels_mut(side).remove(&price);
            }
        }
        self.orders.remove(h);
    }

    #[inline]
    fn bbo(&self) -> (Option<(i64,i64)>, Option<(i64,i64)>) {
        let bid = self.bids.iter().next_back().map(|(p,l)| (*p, l.total_qty));
        let ask = self.asks.iter().next().map(|(p,l)| (*p, l.total_qty));
        (bid, ask)
    }

    #[allow(dead_code)]
    fn top_n(&self, n: usize) -> (SmallVec<[(i64,i64); 32]>, SmallVec<[(i64,i64); 32]>) {
        let mut bids = SmallVec::<[(i64,i64); 32]>::new();
        let mut asks = SmallVec::<[(i64,i64); 32]>::new();
        for (p,l) in self.bids.iter().rev().take(n) { bids.push((*p, l.total_qty)); }
        for (p,l) in self.asks.iter().take(n) { asks.push((*p, l.total_qty)); }
        (bids, asks)
    }
}

pub struct OrderBook {
    _depth_for_reporting: usize,
    books: HashMap<u32, InstrumentBook>,
    index: HashMap<u64, (u32, Handle)>,
    last_instr: Option<u32>,
    consume_trades: bool,
}

impl OrderBook {
    pub fn new(depth_for_reporting: usize) -> Self {
        Self {
            _depth_for_reporting: depth_for_reporting,
            books: HashMap::new(),
            index: HashMap::new(),
            last_instr: None,
            consume_trades: false,
        }
    }

    #[allow(dead_code)]
    pub fn new_with_options(depth_for_reporting: usize, consume_trades: bool) -> Self {
        Self {
            _depth_for_reporting: depth_for_reporting,
            books: HashMap::new(),
            index: HashMap::new(),
            last_instr: None,
            consume_trades,
        }
    }

    pub fn set_consume_trades(&mut self, v: bool) {
        self.consume_trades = v;
    }

    #[inline]
    fn book_mut(&mut self, instr: u32) -> &mut InstrumentBook {
        self.books.entry(instr).or_default()
    }

    pub fn apply(&mut self, ev: &Event) {
        match *ev {
            Event::Add { order_id, instr, px, qty, side } => {
                let book = self.book_mut(instr);
                let h = book.add(order_id, px, qty, side);
                self.index.insert(order_id, (instr, h));
                self.last_instr = Some(instr);
            }
            Event::Mod { order_id, qty } => {
                if let Some((instr, h)) = self.index.get(&order_id).cloned() {
                    let book = self.book_mut(instr);
                    if qty > 0 {
                        book.set_qty(h, qty);
                    } else {
                        book.cancel(h);
                        self.index.remove(&order_id);
                    }
                    self.last_instr = Some(instr);
                }
            }
            Event::Del { order_id } => {
                if let Some((instr, h)) = self.index.remove(&order_id) {
                    let book = self.book_mut(instr);
                    book.cancel(h);
                    self.last_instr = Some(instr);
                }
            }
            Event::Trade { instr, qty, maker_order_id, .. } => {
                self.last_instr = Some(instr);
                if self.consume_trades {
                    if let Some(oid) = maker_order_id {
                        if let Some((mi, h)) = self.index.get(&oid).cloned() {
                            let book = self.book_mut(mi);
                            let new_qty = {
                                let n = &book.orders[h];
                                (n.qty - qty).max(0)
                            };
                            if new_qty > 0 {
                                book.set_qty(h, new_qty);
                            } else {
                                book.cancel(h);
                                self.index.remove(&oid);
                            }
                        }
                    }
                }
            }
            Event::Heartbeat => {}
        }
    }

    pub fn bbo(&self) -> (Option<(i64,i64)>, Option<(i64,i64)>) {
        if let Some(instr) = self.last_instr {
            if let Some(b) = self.books.get(&instr) {
                return b.bbo();
            }
        }
        (None, None)
    }

    #[allow(dead_code)]
    pub fn top_n_of(&self, instr: u32, n: usize) -> Option<(SmallVec<[(i64,i64); 32]>, SmallVec<[(i64,i64); 32]>)> {
        self.books.get(&instr).map(|b| b.top_n(n))
    }

    pub fn order_count(&self) -> usize { self.index.len() }

    // ---------- Snapshot Export/Import ----------

    pub fn export(&self) -> BookExport {
        let mut instruments = Vec::with_capacity(self.books.len());
        for (instr, book) in self.books.iter() {
            let mut orders = Vec::with_capacity(book.orders.len());
            // Bids: best->worst (desc), FIFO per level
            for (price, lvl) in book.bids.iter().rev() {
                for h in lvl.iter_fifo(&book.orders) {
                    let n = &book.orders[h];
                    orders.push(OrderExport {
                        order_id: n.order_id,
                        price: *price,
                        qty: n.qty,
                        side: Side::Bid,
                    });
                }
            }
            // Asks: best->worst (asc), FIFO per level
            for (price, lvl) in book.asks.iter() {
                for h in lvl.iter_fifo(&book.orders) {
                    let n = &book.orders[h];
                    orders.push(OrderExport {
                        order_id: n.order_id,
                        price: *price,
                        qty: n.qty,
                        side: Side::Ask,
                    });
                }
            }
            instruments.push(InstrumentExport { instr: *instr, orders });
        }
        BookExport { version: 1, instruments }
    }

    pub fn from_export(exp: BookExport) -> Self {
        let mut ob = OrderBook::new(10);
        for ie in exp.instruments {
            for o in ie.orders {
                let book = ob.book_mut(ie.instr);
                let h = book.add(o.order_id, o.price, o.qty, o.side);
                ob.index.insert(o.order_id, (ie.instr, h));
            }
            ob.last_instr = Some(ie.instr);
        }
        ob
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fifo_within_level_and_totals() {
        let mut b = InstrumentBook::new();
        let h1 = b.add(1, 100, 10, Side::Bid);
        let h2 = b.add(2, 100, 20, Side::Bid);
        let lvl = b.bids.get(&100).unwrap();
        let mut it = lvl.iter_fifo(&b.orders);
        assert_eq!(it.next(), Some(h1));
        assert_eq!(it.next(), Some(h2));
        assert_eq!(lvl.total_qty, 30);

        b.set_qty(h1, 5);
        let lvl = b.bids.get(&100).unwrap();
        assert_eq!(lvl.total_qty, 25);

        b.cancel(h2);
        let lvl = b.bids.get(&100).unwrap();
        assert_eq!(lvl.total_qty, 5);
        assert_eq!(lvl.count, 1);
    }

    #[test]
    fn remove_empty_levels() {
        let mut b = InstrumentBook::new();
        let h1 = b.add(1, 101, 10, Side::Ask);
        b.cancel(h1);
        assert!(b.asks.get(&101).is_none());
    }
}

/// Serializable snapshot format (coarse-grained; not in hot path).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookExport {
    pub version: u32,
    pub instruments: Vec<InstrumentExport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstrumentExport {
    pub instr: u32,
    pub orders: Vec<OrderExport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderExport {
    pub order_id: u64,
    pub price: i64,
    pub qty: i64,
    pub side: Side,
}