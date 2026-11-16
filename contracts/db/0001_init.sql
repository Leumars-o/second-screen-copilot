-- Second Screen Copilot — local store, migration 0001 (contracts-v1).
-- Rule: no column stores raw audio or images. Enforced by ssc-core's schema test.
-- Times are unix ms (INTEGER). Ids are lowercase hyphenated UUID strings (TEXT).
PRAGMA foreign_keys = ON;

CREATE TABLE schema_version (version INTEGER NOT NULL);
INSERT INTO schema_version (version) VALUES (1);

-- Layer 1: About Me (single row).
CREATE TABLE about_me (
  id          INTEGER PRIMARY KEY CHECK (id = 1),
  name        TEXT NOT NULL DEFAULT '',
  role        TEXT NOT NULL DEFAULT '',
  company     TEXT NOT NULL DEFAULT '',
  product     TEXT NOT NULL DEFAULT '',
  tone        TEXT NOT NULL DEFAULT '',
  preferred_answers TEXT NOT NULL DEFAULT '',
  updated_at  INTEGER NOT NULL
);

-- Layer 2: Profiles and their docs.
CREATE TABLE profiles (
  id           TEXT PRIMARY KEY,          -- slug, e.g. 'sales-product-x'
  name         TEXT NOT NULL,
  instructions TEXT NOT NULL DEFAULT '',
  created_at   INTEGER NOT NULL,
  updated_at   INTEGER NOT NULL
);

CREATE TABLE docs (
  id          TEXT PRIMARY KEY,
  profile_id  TEXT NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
  name        TEXT NOT NULL,               -- shown as source_doc on answer cards
  mime        TEXT NOT NULL,
  sha256      TEXT NOT NULL,
  added_at    INTEGER NOT NULL,
  UNIQUE (profile_id, sha256)
);

CREATE TABLE chunks (
  rowid       INTEGER PRIMARY KEY,         -- joins to vec_chunks.rowid (0002)
  id          TEXT NOT NULL UNIQUE,
  doc_id      TEXT NOT NULL REFERENCES docs(id) ON DELETE CASCADE,
  ord         INTEGER NOT NULL,
  text        TEXT NOT NULL,
  tokens      INTEGER NOT NULL
);
CREATE INDEX chunks_doc ON chunks(doc_id);

-- Layer 3: Contacts and accounts (written only after the user confirms).
CREATE TABLE accounts (
  id          TEXT PRIMARY KEY,
  name        TEXT NOT NULL,
  notes       TEXT NOT NULL DEFAULT '',
  updated_at  INTEGER NOT NULL
);

CREATE TABLE contacts (
  id          TEXT PRIMARY KEY,
  account_id  TEXT REFERENCES accounts(id) ON DELETE SET NULL,
  name        TEXT NOT NULL,
  notes       TEXT NOT NULL DEFAULT '',  -- past objections, commitments, open items
  updated_at  INTEGER NOT NULL
);

CREATE TABLE pending_contact_suggestions (
  id          TEXT PRIMARY KEY,
  session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  name        TEXT NOT NULL,
  company     TEXT,
  notes       TEXT NOT NULL DEFAULT '',
  created_at  INTEGER NOT NULL
);

-- Layer 4: Session history.
CREATE TABLE sessions (
  id          TEXT PRIMARY KEY,
  profile_id  TEXT REFERENCES profiles(id) ON DELETE SET NULL,
  mode        TEXT NOT NULL CHECK (mode IN ('cloud', 'local')),
  started_at  INTEGER NOT NULL,
  ended_at    INTEGER
);

CREATE TABLE session_items (
  id          TEXT PRIMARY KEY,
  session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  kind        TEXT NOT NULL CHECK (kind IN ('summary', 'action_item', 'qa', 'followup_email')),
  body        TEXT NOT NULL,
  created_at  INTEGER NOT NULL
);
CREATE INDEX session_items_session ON session_items(session_id);

-- Only written when the user turned "keep transcripts" on.
CREATE TABLE transcripts (
  id          INTEGER PRIMARY KEY,
  session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  speaker     TEXT NOT NULL CHECK (speaker IN ('me', 'them')),
  text        TEXT NOT NULL,
  t0          INTEGER NOT NULL,
  t1          INTEGER NOT NULL
);

CREATE TABLE answers (
  id          TEXT PRIMARY KEY,
  session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  trigger     TEXT NOT NULL,
  format      TEXT NOT NULL,
  intent      TEXT NOT NULL,
  model       TEXT NOT NULL,
  question    TEXT NOT NULL DEFAULT '',
  body        TEXT NOT NULL,                -- final cue JSON (answer.end payload)
  pinned      INTEGER NOT NULL DEFAULT 0,
  created_at  INTEGER NOT NULL
);

CREATE TABLE answer_feedback (
  answer_id   TEXT NOT NULL REFERENCES answers(id) ON DELETE CASCADE,
  rating      TEXT NOT NULL CHECK (rating IN ('up', 'down')),
  created_at  INTEGER NOT NULL,
  PRIMARY KEY (answer_id)
);
