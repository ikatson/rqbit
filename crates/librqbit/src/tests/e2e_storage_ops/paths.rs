//! Moves and renames against the paths a filesystem really has: other spellings of the
//! same folder (symlinks, `..`, relative paths, case), destinations that already hold other
//! bytes, files deleted under a loaded torrent, and names that collide with files that are
//! not part of the torrent.
