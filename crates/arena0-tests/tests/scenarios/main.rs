//! Program scenarios. Each module drives one bundled program through real
//! Hosts, SQLite stores, and `LocalTransport`, then checks the agreed outcome
//! and every participant's receipt. The modules share one test binary, so the
//! harness and its dependencies link once.

mod chess_bilateral;
mod contract_net_multiparty;
mod cumulative_sum_trilateral;
mod prisoner_dilemma_bilateral;
mod rock_paper_scissors_bilateral;
mod sequential_count_multiparty;
mod vickrey_auction_multiparty;
