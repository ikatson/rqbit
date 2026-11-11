//! Storage operations against the life of the torrent and of the session: a caller that
//! stops waiting, `Session::stop`, a crash between moving and persisting, a torrent in error
//! or waiting for its check, delete and restart during a recheck.
