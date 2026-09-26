use std::fmt::Write;
use std::ops::Not;

use arena0::prelude::*;
use arena0_primitives::turn_manager::TurnManager;
use cozy_chess::util::{display_san_move, display_uci_move, parse_uci_move};
use cozy_chess::{
    Board as CozyBoard, Color as CozyColor, File as CozyFile, Move as CozyMove, Piece as CozyPiece,
    Rank as CozyRank, Square as CozySquare,
};
use strum::Display;

/// Why a side won the game.
#[arena0::data]
#[derive(Copy)]
pub enum WinReason {
    Checkmate,
}

/// Why the game ended in a draw.
#[arena0::data]
#[derive(Copy)]
pub enum DrawReason {
    Stalemate,
    FiftyMoveRule,
    InsufficientMaterial,
}

/// Derived terminal receipt: a pure projection from final shared state.
///
/// Computed in absolute participant order: `winner` is the canonical session
/// participant for the winning color (White is participant 0, Black is
/// participant 1), so every party derives the identical outcome from the agreed
/// final `Status`. A non-terminal `Status::InProgress` cannot reach `outcome`
/// because the session only ends once the board is terminal.
#[arena0::outcome]
pub enum Outcome {
    Win {
        winner: Participant,
        reason: WinReason,
    },
    Draw {
        reason: DrawReason,
    },
}

#[arena0::program(
    name = "chess",
    display_name = "Chess",
    version = "1.0.0",
    description = "Two-player chess with full rule enforcement",
    participants = 2,
    capabilities(auto)
)]
pub mod chess {
    use super::*;
    use arena0::ProgramTransition;

    // Chess declares its SDK-facing types in-module (below); the module-shell
    // macro resolves them directly. Only `Outcome` lives at file scope, so it is
    // the lone alias the program needs.
    type Outcome = super::Outcome;

    #[arena0::data]
    #[derive(Display, Copy)]
    #[strum(serialize_all = "lowercase")]
    pub enum Color {
        White,
        Black,
    }

    impl Not for Color {
        type Output = Self;
        fn not(self) -> Self {
            match self {
                Color::White => Color::Black,
                Color::Black => Color::White,
            }
        }
    }

    impl From<CozyColor> for Color {
        fn from(c: CozyColor) -> Self {
            match c {
                CozyColor::White => Color::White,
                CozyColor::Black => Color::Black,
            }
        }
    }

    impl From<bool> for Color {
        fn from(is_first: bool) -> Self {
            if is_first { Self::White } else { Self::Black }
        }
    }

    impl From<Participant> for Color {
        fn from(participant: Participant) -> Self {
            Self::from(participant.index() == 0)
        }
    }

    impl From<Color> for Participant {
        fn from(color: Color) -> Self {
            match color {
                Color::White => Participant::new(0),
                Color::Black => Participant::new(1),
            }
        }
    }

    #[arena0::data]
    #[derive(Copy, Default)]
    pub enum Status {
        #[default]
        InProgress,
        Checkmate {
            winner: Color,
        },
        Stalemate,
        DrawBy50MoveRule,
        DrawByInsufficientMaterial,
    }

    #[arena0::callouts]
    pub enum Callout {
        /// Make a chess move in UCI notation (e.g. e2e4)
        MakeMove { fen: String, legal_moves: String },
    }

    #[arena0::message]
    pub enum Message {
        Move(String),
    }

    // No terminal phase: in the outcome contract the session ends via
    // `Transition::End` (which derives the outcome and emits `SessionEnd`), not
    // by moving to a "finished" phase. `Playing` is the last program phase; the
    // terminal signal is session-end plus the derived `Outcome` receipt. A move
    // arriving after the board is terminal is still rejected because `Status` in
    // shared state is no longer `InProgress`.
    #[arena0::phases]
    pub enum Phase {
        #[phase(default, description = "Waiting for initialization")]
        Setup,
        #[phase(description = "Game in progress")]
        Playing,
    }

    #[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
    pub enum Error {
        #[error("game is already over")]
        GameOver,
        #[error("move must be 4-5 characters (e.g. e2e4 or e7e8q)")]
        InvalidFormat,
        #[error("game not initialized")]
        GameNotInitialized,
        #[error("illegal move: {0}")]
        IllegalMove(String),
    }

    fn is_insufficient_material(board: &CozyBoard) -> bool {
        let white = board.colors(CozyColor::White);
        let black = board.colors(CozyColor::Black);
        let white_count = white.len();
        let black_count = black.len();
        if white_count == 1 && black_count == 1 {
            return true;
        }
        let minors = board.pieces(CozyPiece::Bishop) | board.pieces(CozyPiece::Knight);
        if white_count == 2 && black_count == 1 && (white & minors).len() == 1 {
            return true;
        }
        if black_count == 2 && white_count == 1 && (black & minors).len() == 1 {
            return true;
        }
        false
    }

    pub fn collect_legal_moves(board: &CozyBoard) -> Vec<cozy_chess::Move> {
        let mut moves = Vec::new();
        board.generate_moves(|piece_moves| {
            moves.extend(piece_moves);
            false
        });
        moves
    }

    /// Participant-local chess state.
    #[arena0::local]
    #[derive(Default)]
    pub struct Local {
        /// Set while this participant's move is queued but not yet applied, so
        /// the callout does not re-ask the answered question.
        move_pending: bool,
    }

    #[arena0::state(max = 32768)]
    pub struct Shared {
        #[phase]
        phase: Phase,
        turns: Option<TurnManager>,
        fen: String,
        status: Status,
        move_history: Vec<String>,
    }

    /// Pure projection from final shared state; no context, effects, or entropy.
    ///
    /// Every party recomputes this from the agreed terminal `Status`, so all
    /// derive the identical receipt. `Status::InProgress` is unreachable here:
    /// the session only ends once the board reaches a terminal status.
    fn outcome(state: &Shared) -> Outcome {
        match state.status {
            Status::Checkmate { winner } => Outcome::Win {
                winner: Participant::from(winner),
                reason: WinReason::Checkmate,
            },
            Status::Stalemate => Outcome::Draw {
                reason: DrawReason::Stalemate,
            },
            Status::DrawBy50MoveRule => Outcome::Draw {
                reason: DrawReason::FiftyMoveRule,
            },
            Status::DrawByInsufficientMaterial => Outcome::Draw {
                reason: DrawReason::InsufficientMaterial,
            },
            // Unreachable: the session only ends from a terminal status. Project
            // a stalemate draw rather than panic so the receptor stays total.
            Status::InProgress => Outcome::Draw {
                reason: DrawReason::Stalemate,
            },
        }
    }

    fn writer(state: &Shared) -> Option<Participant> {
        if state.phase() != Phase::Playing || state.status != Status::InProgress {
            return None;
        }
        state.turns.as_ref().map(TurnManager::current)
    }

    fn view(state: &Shared, _ensemble: &Ensemble, vp: &Viewport) -> View {
        let board = state.current_board();
        // Read-only projections receive only shared state. Render the board in
        // canonical White-first orientation rather than depending on the local
        // replica identity.
        let me = None;
        let viewer = Color::White;
        let header = match board.as_ref() {
            Some(board) => format!("Chess - {}", state.status.line(Some(board))),
            None => "Chess - waiting for game to start".to_string(),
        };

        View::new()
            .header(vp.fit_text(header))
            .agents(vp.fit_text(render_agents(board.as_ref(), me)))
            .state(vp.fit_text(render_board_state(state, board.as_ref(), viewer, vp)))
            .status_bar(vp.fit_text(render_status(&state.status, board.as_ref())))
    }

    fn render_agents(board: Option<&CozyBoard>, me: Option<usize>) -> String {
        let mut agents = String::new();
        let white_captures =
            board.map_or_else(String::new, |board| captured_by(board, Color::White));
        let black_captures =
            board.map_or_else(String::new, |board| captured_by(board, Color::Black));
        let _ = writeln!(
            agents,
            "{} white: captured {}",
            player_label(0, me),
            empty_dash(&white_captures)
        );
        let _ = writeln!(
            agents,
            "{} black: captured {}",
            player_label(1, me),
            empty_dash(&black_captures)
        );
        agents
    }

    fn render_board_state(
        state: &Shared,
        board: Option<&CozyBoard>,
        viewer: Color,
        vp: &Viewport,
    ) -> String {
        let Some(board) = board else {
            return "Waiting for game to start".to_string();
        };

        let mut body = oriented_board(board, viewer, last_move_squares(&state.move_history), vp);
        if let Some(last) = state.move_history.last() {
            let _ = write!(body, "\nLast move: {last}");
        }
        body
    }

    fn render_status(status: &Status, board: Option<&CozyBoard>) -> String {
        if !matches!(status, Status::InProgress) {
            return status.game_result();
        }

        let mut line = status.line(board);
        if board.is_some_and(|board| !board.checkers().is_empty()) {
            line.push_str(" - check");
        }
        line
    }

    fn oriented_board(
        board: &CozyBoard,
        viewer: Color,
        highlight: Option<(CozySquare, CozySquare)>,
        vp: &Viewport,
    ) -> String {
        const SEPARATOR: &str = "  +---+---+---+---+---+---+---+---+\n";

        let files = oriented_files(viewer);
        let ranks = oriented_ranks(viewer);
        let mut s = String::with_capacity(512);

        s.push_str("   ");
        for file in &files {
            let _ = write!(s, " {file}  ");
        }
        s.push('\n');
        s.push_str(SEPARATOR);

        for rank in ranks {
            let _ = write!(s, "{rank} |");
            for &file in &files {
                let sq = CozySquare::new(file, rank);
                let ch = board
                    .piece_on(sq)
                    .zip(board.color_on(sq))
                    .map(|(piece, color)| unicode_piece(piece, color))
                    .unwrap_or('.');
                let text = format!(" {ch} ");
                if highlight.is_some_and(|(from, to)| sq == from || sq == to)
                    && vp.color.supports_color()
                {
                    let _ = write!(s, "\x1b[43;30m{text}\x1b[0m|");
                } else {
                    let _ = write!(s, "{text}|");
                }
            }
            let _ = writeln!(s, " {rank}");
            s.push_str(SEPARATOR);
        }

        s.push_str("   ");
        for file in &files {
            let _ = write!(s, " {file}  ");
        }
        s
    }

    fn oriented_files(viewer: Color) -> Vec<CozyFile> {
        let mut files = CozyFile::ALL.to_vec();
        if viewer == Color::Black {
            files.reverse();
        }
        files
    }

    fn oriented_ranks(viewer: Color) -> Vec<CozyRank> {
        let mut ranks = CozyRank::ALL.to_vec();
        if viewer == Color::White {
            ranks.reverse();
        }
        ranks
    }

    fn last_move_squares(history: &[String]) -> Option<(CozySquare, CozySquare)> {
        let mut board = CozyBoard::default();
        let mut last = None;
        for san in history {
            let mv = san_move(&board, san)?;
            last = Some((mv.from, mv.to));
            board.play(mv);
        }
        last
    }

    fn san_move(board: &CozyBoard, san: &str) -> Option<CozyMove> {
        collect_legal_moves(board)
            .into_iter()
            .find(|&mv| format!("{}", display_san_move(board, mv)) == san)
    }

    fn captured_by(board: &CozyBoard, player: Color) -> String {
        let captured_color = match player {
            Color::White => CozyColor::Black,
            Color::Black => CozyColor::White,
        };
        let start = CozyBoard::default();
        let mut pieces = String::new();
        for piece in [
            CozyPiece::Pawn,
            CozyPiece::Knight,
            CozyPiece::Bishop,
            CozyPiece::Rook,
            CozyPiece::Queen,
        ] {
            let initial = piece_count(&start, captured_color, piece);
            let current = piece_count(board, captured_color, piece);
            for _ in 0..initial.saturating_sub(current) {
                pieces.push(unicode_piece(piece, captured_color));
            }
        }
        pieces
    }

    fn piece_count(board: &CozyBoard, color: CozyColor, piece: CozyPiece) -> u32 {
        (board.colors(color) & board.pieces(piece)).len()
    }

    fn empty_dash(s: &str) -> &str {
        if s.is_empty() { "-" } else { s }
    }

    fn player_label(idx: usize, me: Option<usize>) -> String {
        // Role first, seat index second (L047): the human always reads
        // "you"/"opponent" first no matter which seat they hold.
        match me {
            Some(me) if idx == me => format!("you (P{idx})"),
            Some(_) => format!("opponent (P{idx})"),
            None => format!("P{idx}"),
        }
    }

    /// Position-0 boundary: set up the starting board and the turn order. The resulting
    /// state determines the mover's question.
    fn on_session_started(
        ctx: &mut Context<Shared, Local>,
    ) -> Result<ProgramTransition<Chess>, ProgramFault> {
        let participants = vec![Participant::new(0), Participant::new(1)];
        ctx.mutate_shared(|state| {
            let board = CozyBoard::default();
            state.sync_from_board(&board);
            state.move_history.clear();
            state.turns = Some(TurnManager::new(participants));
        });
        Ok(Transition::To(Phase::Playing))
    }

    /// Ask the participant whose turn it is for a legal move.
    fn callout(ctx: &CalloutContext<Shared, Local>) -> Option<Callout> {
        let state = ctx.shared();
        if state.phase() != Phase::Playing || state.status != Status::InProgress {
            return None;
        }
        // An answer already queued this participant's move; do not re-ask the
        // same question until the author's own message is applied.
        if ctx.local().move_pending {
            return None;
        }
        let is_my_turn = state
            .turns
            .as_ref()
            .is_some_and(|turns| turns.current() == ctx.me());
        if !is_my_turn {
            return None;
        }
        let fen = state.fen.clone();
        let legal_moves = state.legal_moves_string();
        Some(callouts::MakeMove { fen, legal_moves }.into())
    }

    fn on_input(ctx: &mut LocalContext<Shared, Local>, input: Input) -> arena0::anyhow::Result<()> {
        let Input::MakeMove(text) = input;
        let move_str = text.trim();
        if writer(ctx.shared()) != Some(ctx.me()) {
            return Err(anyhow!("this participant does not own the next move"));
        }
        // Validate the move against the committed board without mutating
        // shared state; the author applies it when its own message is applied.
        ctx.shared()
            .validate_move(move_str)
            .map_err(|e| anyhow!(e))?;
        ctx.mutate_local(|local| local.move_pending = true);
        ctx.effects()
            .broadcast(&Message::Move(move_str.to_string()))?;
        Ok(())
    }

    fn on_message(
        ctx: &mut Context<Shared, Local>,
        from: Participant,
        msg: Message,
    ) -> MessageApply<Chess> {
        if writer(ctx.shared()) != Some(from) {
            return Ok(ApplyDecision::Reject);
        }
        let Message::Move(text) = msg;
        let move_str = text.trim();
        let Ok(finished) = apply_move(ctx.shared_mut(), move_str) else {
            return Ok(ApplyDecision::Reject);
        };
        ctx.mutate_local(|local| local.move_pending = false);
        if finished {
            return Ok(ApplyDecision::Accept(Transition::End));
        }
        // The resulting turn determines the next mover's callout.
        Ok(ApplyDecision::Accept(Transition::Stay))
    }

    fn on_query(_shared: &Shared, _: ()) {}

    /// Apply one validated move to the shared board and advance the canonical
    /// turn. Every participant, including the author applying its own message,
    /// uses this helper.
    fn apply_move(state: &mut Shared, move_str: &str) -> Result<bool, Error> {
        let (board, algebraic) = state.validate_move(move_str)?;
        state.apply_validated_move(&board, algebraic);
        state.turns.as_mut().expect("turns initialized").advance();
        Ok(state.status != Status::InProgress)
    }

    impl Shared {
        fn current_board(&self) -> Option<CozyBoard> {
            if self.fen.is_empty() {
                None
            } else {
                Some(
                    self.fen
                        .parse::<CozyBoard>()
                        .expect("stored FEN should parse"),
                )
            }
        }

        fn legal_moves_string(&self) -> String {
            let Some(board) = self.current_board() else {
                return String::new();
            };
            let mut moves: Vec<String> = collect_legal_moves(&board)
                .into_iter()
                .map(|mv| format!("{}", display_uci_move(&board, mv)))
                .collect();
            moves.sort();
            moves.join(", ")
        }

        fn sync_from_board(&mut self, board: &CozyBoard) {
            self.fen = board.to_string();
            self.status = Status::compute(board);
        }

        fn apply_validated_move(&mut self, board: &CozyBoard, algebraic: String) {
            self.sync_from_board(board);
            self.move_history.push(algebraic);
        }

        fn validate_move(&self, move_str: &str) -> Result<(CozyBoard, String), Error> {
            if self.phase() != Phase::Playing {
                return Err(Error::GameOver);
            }
            if self.status != Status::InProgress {
                return Err(Error::GameOver);
            }
            let clean = move_str.trim().to_lowercase();
            if clean.len() < 4 || clean.len() > 5 {
                return Err(Error::InvalidFormat);
            }
            let mut board = self.current_board().ok_or(Error::GameNotInitialized)?;
            let mv =
                parse_uci_move(&board, &clean).map_err(|_| Error::IllegalMove(clean.clone()))?;
            if !board.is_legal(mv) {
                return Err(Error::IllegalMove(clean));
            }
            let algebraic = format!("{}", display_san_move(&board, mv));
            board.play(mv);
            Ok((board, algebraic))
        }
    }

    fn unicode_piece(piece: CozyPiece, color: CozyColor) -> char {
        const WHITE_PIECES: [char; CozyPiece::NUM] = ['♙', '♘', '♗', '♖', '♕', '♔'];
        const BLACK_PIECES: [char; CozyPiece::NUM] = ['♟', '♞', '♝', '♜', '♛', '♚'];

        match color {
            CozyColor::White => WHITE_PIECES[piece as usize],
            CozyColor::Black => BLACK_PIECES[piece as usize],
        }
    }

    pub fn ascii_board(board: &CozyBoard) -> String {
        const HEADER: &str = "    a   b   c   d   e   f   g   h\n";
        const SEPARATOR: &str = "  +---+---+---+---+---+---+---+---+\n";

        let mut s = String::with_capacity(512);
        s.push_str(HEADER);
        s.push_str(SEPARATOR);

        for (display_rank, &rank) in (1..=8).rev().zip(CozyRank::ALL.iter().rev()) {
            let _ = write!(s, "{display_rank} |");
            for &file in &CozyFile::ALL {
                let sq = CozySquare::new(file, rank);
                let ch = board
                    .piece_on(sq)
                    .zip(board.color_on(sq))
                    .map(|(piece, color)| unicode_piece(piece, color))
                    .unwrap_or('.');
                let _ = write!(s, " {ch} |");
            }
            let _ = writeln!(s, " {display_rank}");
            s.push_str(SEPARATOR);
        }
        s.push_str(HEADER);
        s
    }

    pub fn move_history_text(history: &[String]) -> String {
        let mut result = String::new();
        for (i, mv) in history.iter().enumerate() {
            if i % 2 == 0 {
                if !result.is_empty() {
                    result.push(' ');
                }
                let _ = write!(result, "{}.", i / 2 + 1);
            }
            result.push(' ');
            result.push_str(mv);
        }
        result
    }

    impl Status {
        /// Derive the game status from the current board.
        pub fn compute(board: &CozyBoard) -> Self {
            if !board.generate_moves(|_| true) {
                return if board.checkers().is_empty() {
                    Self::Stalemate
                } else {
                    Self::Checkmate {
                        winner: Color::from(!board.side_to_move()),
                    }
                };
            }
            if board.halfmove_clock() >= 100 {
                return Self::DrawBy50MoveRule;
            }
            if is_insufficient_material(board) {
                return Self::DrawByInsufficientMaterial;
            }
            Self::InProgress
        }

        /// The one-line status shown while the game is in progress.
        pub fn line(&self, board: Option<&CozyBoard>) -> String {
            match (self, board) {
                (Status::InProgress, Some(board)) => {
                    let color = Color::from(board.side_to_move());
                    format!("{color}'s turn, move {}", board.fullmove_number())
                }
                (Status::InProgress, None) => "Waiting for game to start".into(),
                (Status::Checkmate { winner }, _) => format!("Checkmate! {winner} wins"),
                (Status::Stalemate, _) => "Draw by stalemate".into(),
                (Status::DrawBy50MoveRule, _) => "Draw by fifty-move rule".into(),
                (Status::DrawByInsufficientMaterial, _) => "Draw by insufficient material".into(),
            }
        }

        /// The terminal result line, also shown for an in-progress game.
        pub fn game_result(&self) -> String {
            match self {
                Status::Checkmate { winner } => format!("Checkmate! {winner} wins."),
                Status::Stalemate => "Draw by stalemate.".into(),
                Status::DrawBy50MoveRule => "Draw by fifty-move rule.".into(),
                Status::DrawByInsufficientMaterial => "Draw by insufficient material.".into(),
                Status::InProgress => "Game in progress.".into(),
            }
        }
    }

    /// Terminal projection coverage: every ruled terminal board renders all
    /// four slots and projects its exact receipt. Expectations below are
    /// specified literals; the boards reach them through real legal moves.
    #[cfg(test)]
    mod projection_tests {
        use super::super::{DrawReason, WinReason};
        use super::*;
        use arena0::types::{ColorDepth, Slot};

        #[test]
        fn terminal_view_and_outcome_preserve_chess_results() {
            // Play UCI moves legally, recording SAN history as `validate_move` does.
            let play = |mut board: CozyBoard, moves: &[&str]| -> (CozyBoard, Vec<String>) {
                let mut history = Vec::new();
                for uci in moves {
                    let mv = parse_uci_move(&board, uci).expect("fixture move parses");
                    assert!(board.is_legal(mv), "{uci} is legal");
                    history.push(format!("{}", display_san_move(&board, mv)));
                    board.play(mv);
                }
                (board, history)
            };
            let (scholar, scholar_history) = play(
                CozyBoard::default(),
                &["e2e4", "e7e5", "d1h5", "b8c6", "f1c4", "g8f6", "h5f7"],
            );
            let (stalemate, stalemate_history) = play(
                "7k/8/4Q3/6K1/8/8/8/8 w - - 0 1".parse().expect("valid FEN"),
                &["e6f7"],
            );
            let (fifty, fifty_history) = play(
                "4k3/8/8/8/8/8/4K2R/8 w - - 99 1"
                    .parse()
                    .expect("valid FEN"),
                &["h2h3"],
            );
            let (material, material_history) = play(
                "4k3/7b/8/8/8/8/2B1K3/8 w - - 0 1"
                    .parse()
                    .expect("valid FEN"),
                &["c2h7"],
            );
            let cases: Vec<(&str, CozyBoard, Vec<String>, Status, &str)> = vec![
                (
                    "scholar's mate",
                    scholar,
                    scholar_history,
                    Status::Checkmate {
                        winner: Color::White,
                    },
                    "Checkmate! white wins.",
                ),
                (
                    "stalemate",
                    stalemate,
                    stalemate_history,
                    Status::Stalemate,
                    "Draw by stalemate.",
                ),
                (
                    "fifty-move rule",
                    fifty,
                    fifty_history,
                    Status::DrawBy50MoveRule,
                    "Draw by fifty-move rule.",
                ),
                (
                    "insufficient material",
                    material,
                    material_history,
                    Status::DrawByInsufficientMaterial,
                    "Draw by insufficient material.",
                ),
            ];
            let ensemble = Ensemble::from_peers(vec![PeerId([0; 32]), PeerId([1; 32])])
                .expect("valid view ensemble");
            for (label, board, history, expected_status, expected_text) in cases {
                // The real rule engine must agree with the specified literal.
                assert_eq!(Status::compute(&board), expected_status, "{label}");
                let state = Shared {
                    fen: board.to_string(),
                    status: expected_status,
                    move_history: history,
                    ..Shared::default()
                };
                let viewport = Viewport {
                    width: 120,
                    color: ColorDepth::Mono,
                };
                let view = <Chess as ProgramView>::view(&state, &ensemble, &viewport);
                assert_eq!(view.slots.len(), 4, "{label} fills every slot");
                for slot in [Slot::Header, Slot::Agents, Slot::State, Slot::StatusBar] {
                    assert!(
                        view.slots.contains_key(&slot),
                        "{label} is missing {slot:?}"
                    );
                }
                assert!(
                    view.slots[&Slot::State].contains('\u{2654}'),
                    "{label} shows the white king"
                );
                assert!(
                    view.slots[&Slot::State].contains('\u{265A}'),
                    "{label} shows the black king"
                );
                assert!(
                    view.slots[&Slot::StatusBar].contains(expected_text),
                    "{label} reports its terminal status"
                );
                assert!(
                    view.slots.values().all(|text| !text.contains("\x1b[")),
                    "{label} mono view must not contain escapes"
                );
                let outcome = <Chess as Program>::outcome(&state);
                match expected_status {
                    Status::Checkmate {
                        winner: Color::White,
                    } => assert!(
                        matches!(outcome, Outcome::Win { winner, reason: WinReason::Checkmate } if winner == Participant::new(0)),
                        "{label} is a white checkmate win"
                    ),
                    Status::Stalemate => assert!(
                        matches!(
                            outcome,
                            Outcome::Draw {
                                reason: DrawReason::Stalemate
                            }
                        ),
                        "{label} is a stalemate draw"
                    ),
                    Status::DrawBy50MoveRule => assert!(
                        matches!(
                            outcome,
                            Outcome::Draw {
                                reason: DrawReason::FiftyMoveRule
                            }
                        ),
                        "{label} is a fifty-move draw"
                    ),
                    Status::DrawByInsufficientMaterial => assert!(
                        matches!(
                            outcome,
                            Outcome::Draw {
                                reason: DrawReason::InsufficientMaterial
                            }
                        ),
                        "{label} is an insufficient-material draw"
                    ),
                    Status::InProgress | Status::Checkmate { .. } => {
                        panic!("{label} is not a specified terminal status")
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::chess::*;
    use super::*;

    #[test]
    fn ascii_board_renders_starting_position() {
        let board = CozyBoard::default();
        let rendered = ascii_board(&board);

        assert!(rendered.starts_with("    a   b   c   d   e   f   g   h\n"));
        assert!(rendered.contains("8 | ♜ | ♞ | ♝ | ♛ | ♚ | ♝ | ♞ | ♜ | 8\n"));
        assert!(rendered.contains("2 | ♙ | ♙ | ♙ | ♙ | ♙ | ♙ | ♙ | ♙ | 2\n"));
        assert!(rendered.ends_with("    a   b   c   d   e   f   g   h\n"));
    }

    #[test]
    fn move_history_groups_moves_by_turn() {
        let history = vec!["e4".to_string(), "e5".to_string(), "Nf3".to_string()];

        assert_eq!(move_history_text(&history), "1. e4 e5 2. Nf3");
    }

    #[test]
    fn status_line_reports_active_color_and_move_number() {
        let board = CozyBoard::default();

        assert_eq!(
            Status::InProgress.line(Some(&board)),
            "white's turn, move 1"
        );
        assert_eq!(Status::InProgress.line(None), "Waiting for game to start");
    }

    #[test]
    fn game_result_formats_terminal_states() {
        assert_eq!(
            Status::Checkmate {
                winner: Color::Black
            }
            .game_result(),
            "Checkmate! black wins."
        );
        assert_eq!(Status::Stalemate.game_result(), "Draw by stalemate.");
    }

    #[test]
    fn draw_rules_classify_terminal_boards() {
        for (fen, uci, expected) in [
            ("7k/8/4Q3/6K1/8/8/8/8 w - - 0 1", "e6f7", Status::Stalemate),
            (
                "4k3/8/8/8/8/8/4K2R/8 w - - 99 1",
                "h2h3",
                Status::DrawBy50MoveRule,
            ),
            (
                "4k3/7b/8/8/8/8/2B1K3/8 w - - 0 1",
                "c2h7",
                Status::DrawByInsufficientMaterial,
            ),
        ] {
            let mut board: CozyBoard = fen.parse().expect("valid FEN");
            let mv = parse_uci_move(&board, uci).expect("valid UCI move");
            assert!(board.is_legal(mv), "{uci} is legal in {fen}");
            board.play(mv);
            assert_eq!(Status::compute(&board), expected, "{fen} then {uci}");
        }
    }

    #[test]
    fn color_participant_mapping_round_trips() {
        for color in [Color::White, Color::Black] {
            let participant = Participant::from(color);
            assert_eq!(Color::from(participant), color);
            assert_eq!(Participant::from(Color::from(participant)), participant);
        }
    }

    #[test]
    fn input_schema_metadata() {
        let schemas = <Callout as Arena0Callout>::schemas();
        assert_eq!(schemas.len(), 1);

        let s = &schemas[0];
        assert_eq!(s.name, "MakeMove");
        assert!(!s.prompt.is_empty());
        assert_eq!(s.output.as_value()["type"], "string");
        assert_eq!(
            s.input.as_value()["properties"]
                .as_object()
                .expect("expected object input schema")
                .len(),
            2
        );
    }
}
