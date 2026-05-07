-- Payway initial schema
-- =====================
-- Models the lifecycle of a cross-border payment with a strict double-entry
-- ledger. See learn/schema-design.md for the design rationale.
--
-- Read order:
--   extensions -> enums -> business_entities -> recipients -> accounts
--   -> fx_quotes -> transactions -> transaction_status_history
--   -> ledger_entries -> webhook_events -> idempotency_keys
--   -> triggers -> views

-- ---------------------------------------------------------------------------
-- Extensions
-- ---------------------------------------------------------------------------
CREATE EXTENSION IF NOT EXISTS pgcrypto;  -- gen_random_uuid()

-- ---------------------------------------------------------------------------
-- Enums
-- ---------------------------------------------------------------------------

-- Payment lifecycle. Allowed transitions enforced by trigger below.
--   initiated  -> processing | failed
--   processing -> completed  | failed
--   completed  -> reversed
--   failed, reversed are terminal.
CREATE TYPE transaction_status AS ENUM (
  'initiated',
  'processing',
  'completed',
  'failed',
  'reversed'
);

-- 'user'               : owned by a business client, money belongs to them
-- 'clearing'           : Payway-internal account holding value mid-flight
-- 'external_recipient' : represents a foreign counterparty's bank — money
--                        "leaves" Payway when credited here
-- 'system'             : Payway book-keeping account with no real-world
--                        owner (e.g. external deposits source, FX P&L)
CREATE TYPE account_type AS ENUM (
  'user',
  'clearing',
  'external_recipient',
  'system'
);

CREATE TYPE webhook_processing_status AS ENUM (
  'pending',
  'processed',
  'failed',
  'ignored'
);

-- ---------------------------------------------------------------------------
-- currencies — supported ISO 4217 currencies
-- ---------------------------------------------------------------------------
-- Every currency-bearing column FK-references this table, so an unsupported
-- code (e.g. 'XYZ') cannot enter the system. is_active=false soft-disables a
-- currency for NEW payments without breaking history; application code at
-- payment creation rejects inactive currencies with a clean error.
-- exponent is the ISO 4217 minor-unit exponent (NGN=2, USD=2, JPY=0, BHD=3),
-- kept as metadata for UI formatting even though our amount columns are
-- NUMERIC and don't need it for arithmetic.
CREATE TABLE currencies (
  code       CHAR(3) PRIMARY KEY,
  name       TEXT NOT NULL,
  exponent   SMALLINT NOT NULL,
  is_active  BOOLEAN NOT NULL DEFAULT TRUE,
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),

  CONSTRAINT currency_code_uppercase CHECK (code = UPPER(code) AND length(code) = 3),
  CONSTRAINT currency_exponent_range CHECK (exponent >= 0 AND exponent <= 4)
);

-- ---------------------------------------------------------------------------
-- business_entities — Nigerian businesses + Payway itself
-- ---------------------------------------------------------------------------
CREATE TABLE business_entities (
  id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  name         TEXT NOT NULL,
  country_code CHAR(2) NOT NULL,                       -- ISO 3166-1 alpha-2
  is_internal  BOOLEAN NOT NULL DEFAULT FALSE,         -- TRUE = Payway-owned
  created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- ---------------------------------------------------------------------------
-- recipients — foreign suppliers receiving payment
-- ---------------------------------------------------------------------------
-- We don't custody recipient funds; we just need bank coordinates to pass
-- on to the downstream provider. In production these fields would be
-- encrypted at rest (PII + financial data).
CREATE TABLE recipients (
  id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  name                TEXT NOT NULL,
  country_code        CHAR(2) NOT NULL,
  bank_name           TEXT,
  bank_account_number TEXT,
  created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- ---------------------------------------------------------------------------
-- accounts
-- ---------------------------------------------------------------------------
-- One row per (owner, currency, type). Balance is NEVER stored here — it's
-- derived from ledger_entries (see view at bottom of file). This makes
-- balance/ledger drift impossible by construction.
--
-- The XOR check enforces that owner_business_id and owner_recipient_id are
-- mutually exclusive based on account_type. 'system' accounts have neither.
CREATE TABLE accounts (
  id                 UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  account_type       account_type NOT NULL,
  owner_business_id  UUID REFERENCES business_entities(id),
  owner_recipient_id UUID REFERENCES recipients(id),
  currency           CHAR(3) NOT NULL REFERENCES currencies(code),
  display_name       TEXT,
  created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),

  CONSTRAINT account_owner_matches_type CHECK (
    (account_type IN ('user', 'clearing')
       AND owner_business_id IS NOT NULL
       AND owner_recipient_id IS NULL)
    OR
    (account_type = 'external_recipient'
       AND owner_recipient_id IS NOT NULL
       AND owner_business_id IS NULL)
    OR
    (account_type = 'system'
       AND owner_business_id IS NULL
       AND owner_recipient_id IS NULL)
  )
);

-- One business has at most one (account_type, currency) account.
CREATE UNIQUE INDEX accounts_business_unique
  ON accounts (owner_business_id, account_type, currency)
  WHERE owner_business_id IS NOT NULL;

-- One recipient has at most one external account per currency.
CREATE UNIQUE INDEX accounts_recipient_unique
  ON accounts (owner_recipient_id, currency)
  WHERE owner_recipient_id IS NOT NULL;

-- One clearing account per currency, globally.
CREATE UNIQUE INDEX accounts_clearing_unique
  ON accounts (currency)
  WHERE account_type = 'clearing';

-- ---------------------------------------------------------------------------
-- fx_quotes
-- ---------------------------------------------------------------------------
-- A quote is issued for a base->quote pair with an explicit expiry. Exactly
-- one transaction can lock a given quote (see partial unique index below).
CREATE TABLE fx_quotes (
  id                       UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  base_currency            CHAR(3) NOT NULL REFERENCES currencies(code),
  quote_currency           CHAR(3) NOT NULL REFERENCES currencies(code),
  rate                     NUMERIC(20, 8) NOT NULL,    -- 8 dp for rate precision
  base_amount              NUMERIC(20, 4) NOT NULL,
  quote_amount             NUMERIC(20, 4) NOT NULL,
  issued_at                TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  expires_at               TIMESTAMPTZ NOT NULL,
  locked_by_transaction_id UUID,                       -- FK added below

  CONSTRAINT fx_quote_expires_after_issue CHECK (expires_at > issued_at),
  CONSTRAINT fx_quote_positive_amounts    CHECK (base_amount > 0 AND quote_amount > 0),
  CONSTRAINT fx_quote_positive_rate       CHECK (rate > 0),
  CONSTRAINT fx_quote_currency_diff       CHECK (base_currency <> quote_currency)
);

-- ---------------------------------------------------------------------------
-- transactions
-- ---------------------------------------------------------------------------
-- The customer-facing payment record. Status transitions are constrained
-- by trigger; status changes are auto-recorded into transaction_status_history.
CREATE TABLE transactions (
  id                   UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  reference            TEXT NOT NULL UNIQUE,           -- human-readable, e.g. PWY-2026-000123
  status               transaction_status NOT NULL DEFAULT 'initiated',
  sender_account_id    UUID NOT NULL REFERENCES accounts(id),
  recipient_id         UUID NOT NULL REFERENCES recipients(id),
  source_currency      CHAR(3) NOT NULL REFERENCES currencies(code),
  source_amount        NUMERIC(20, 4) NOT NULL,
  destination_currency CHAR(3) NOT NULL REFERENCES currencies(code),
  destination_amount   NUMERIC(20, 4) NOT NULL,
  fx_quote_id          UUID NOT NULL REFERENCES fx_quotes(id),
  provider_name        TEXT,
  provider_reference   TEXT,
  initiated_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  submitted_at         TIMESTAMPTZ,
  completed_at         TIMESTAMPTZ,
  failed_at            TIMESTAMPTZ,
  failure_reason       TEXT,

  CONSTRAINT tx_positive_amounts      CHECK (source_amount > 0 AND destination_amount > 0),
  CONSTRAINT tx_currency_diff         CHECK (source_currency <> destination_currency),
  CONSTRAINT tx_provider_ref_nonempty CHECK (provider_reference IS NULL OR length(provider_reference) > 0)
);

-- The provider reference is what inbound webhooks key on. Must be unique
-- when present, otherwise we can't route a webhook to a single transaction.
CREATE UNIQUE INDEX transactions_provider_reference_unique
  ON transactions (provider_reference)
  WHERE provider_reference IS NOT NULL;

CREATE INDEX transactions_status_initiated_at ON transactions (status, initiated_at DESC);
CREATE INDEX transactions_sender_account_id   ON transactions (sender_account_id);

-- Now wire fx_quotes -> transactions FK (circular reference resolved here).
ALTER TABLE fx_quotes
  ADD CONSTRAINT fx_quotes_locked_by_fk
  FOREIGN KEY (locked_by_transaction_id) REFERENCES transactions(id);

-- A given quote can be locked by at most one transaction.
CREATE UNIQUE INDEX fx_quotes_locked_unique
  ON fx_quotes (locked_by_transaction_id)
  WHERE locked_by_transaction_id IS NOT NULL;

-- ---------------------------------------------------------------------------
-- transaction_status_history — timeline / audit log
-- ---------------------------------------------------------------------------
CREATE TABLE transaction_status_history (
  id             BIGSERIAL PRIMARY KEY,
  transaction_id UUID NOT NULL REFERENCES transactions(id),
  from_status    transaction_status,                   -- NULL for initial insert
  to_status      transaction_status NOT NULL,
  changed_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  reason         TEXT
);

CREATE INDEX transaction_status_history_tx_id
  ON transaction_status_history (transaction_id, changed_at);

-- ---------------------------------------------------------------------------
-- ledger_entries — the source of truth for money
-- ---------------------------------------------------------------------------
-- Signed amounts: negative = debit, positive = credit.
-- Each entry belongs to either a payment transaction OR a journal (deposits,
-- adjustments, opening balances). Entries grouped by transaction_id or
-- journal_id MUST sum to zero per currency — enforced at commit time by
-- the deferred trigger below.
--
-- The table is APPEND-ONLY. Reversals are new entries with opposite signs,
-- never UPDATE/DELETE on existing rows.
CREATE TABLE ledger_entries (
  id             BIGSERIAL PRIMARY KEY,
  transaction_id UUID REFERENCES transactions(id),
  journal_id     UUID,
  account_id     UUID NOT NULL REFERENCES accounts(id),
  amount         NUMERIC(20, 4) NOT NULL,
  currency       CHAR(3) NOT NULL REFERENCES currencies(code),
  entry_type     TEXT NOT NULL,                        -- e.g. 'debit_sender', 'credit_recipient'
  created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),

  CONSTRAINT ledger_amount_nonzero CHECK (amount <> 0),
  CONSTRAINT ledger_source_xor CHECK (
    (transaction_id IS NOT NULL AND journal_id IS NULL)
    OR
    (transaction_id IS NULL AND journal_id IS NOT NULL)
  )
);

CREATE INDEX ledger_entries_account_id     ON ledger_entries (account_id);
CREATE INDEX ledger_entries_transaction_id ON ledger_entries (transaction_id) WHERE transaction_id IS NOT NULL;
CREATE INDEX ledger_entries_journal_id     ON ledger_entries (journal_id)     WHERE journal_id IS NOT NULL;

-- ---------------------------------------------------------------------------
-- webhook_events — raw log
-- ---------------------------------------------------------------------------
-- Every inbound webhook is persisted here BEFORE any business logic runs.
-- Even malformed, unsigned, or unknown-reference webhooks get a row. This
-- record is the evidence we replay against and the audit trail for disputes.
CREATE TABLE webhook_events (
  id                     UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  provider               TEXT NOT NULL,
  provider_event_id      TEXT,
  raw_payload            BYTEA NOT NULL,               -- raw bytes, not parsed JSON
  headers                JSONB NOT NULL,
  signature              TEXT,
  signature_valid        BOOLEAN,                      -- NULL = not yet verified
  received_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  processed_at           TIMESTAMPTZ,
  processing_status      webhook_processing_status NOT NULL DEFAULT 'pending',
  processing_error       TEXT,
  related_transaction_id UUID REFERENCES transactions(id)
);

-- Same provider event id arriving twice = same event. Partial because some
-- providers don't send an event id; for those we'd dedupe at the app layer.
CREATE UNIQUE INDEX webhook_events_provider_event_unique
  ON webhook_events (provider, provider_event_id)
  WHERE provider_event_id IS NOT NULL;

CREATE INDEX webhook_events_received_at  ON webhook_events (received_at DESC);
CREATE INDEX webhook_events_unprocessed  ON webhook_events (received_at)
  WHERE processed_at IS NULL;

-- ---------------------------------------------------------------------------
-- idempotency_keys
-- ---------------------------------------------------------------------------
-- Stores the ORIGINAL response so replays return byte-identical results.
-- request_hash detects "same key, different body" — a client bug we surface
-- as 422 Unprocessable Entity rather than silently honoring.
-- (scope, key) is the primary key — same key under different endpoints OK.
CREATE TABLE idempotency_keys (
  key             TEXT NOT NULL,
  scope           TEXT NOT NULL,                       -- e.g. 'POST /payments'
  request_hash    TEXT NOT NULL,                       -- hex(sha256(canonical_body))
  response_status INT,
  response_body   JSONB,
  transaction_id  UUID REFERENCES transactions(id),
  created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  expires_at      TIMESTAMPTZ NOT NULL,

  PRIMARY KEY (scope, key)
);

CREATE INDEX idempotency_keys_expires_at ON idempotency_keys (expires_at);

-- ===========================================================================
-- TRIGGERS
-- ===========================================================================

-- ---------------------------------------------------------------------------
-- 1. Reject invalid transaction status transitions
-- ---------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION enforce_transaction_status_transition()
RETURNS TRIGGER AS $$
BEGIN
  IF OLD.status = NEW.status THEN
    RETURN NEW;
  END IF;

  IF NOT (
    (OLD.status = 'initiated'  AND NEW.status IN ('processing', 'failed'))
    OR (OLD.status = 'processing' AND NEW.status IN ('completed',  'failed'))
    OR (OLD.status = 'completed'  AND NEW.status = 'reversed')
  ) THEN
    RAISE EXCEPTION 'invalid transaction status transition: % -> %',
                    OLD.status, NEW.status
      USING ERRCODE = 'check_violation';
  END IF;

  RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER transactions_status_transition_check
  BEFORE UPDATE OF status ON transactions
  FOR EACH ROW
  EXECUTE FUNCTION enforce_transaction_status_transition();

-- ---------------------------------------------------------------------------
-- 2. Auto-record status changes into transaction_status_history
-- ---------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION record_transaction_status_change()
RETURNS TRIGGER AS $$
BEGIN
  IF TG_OP = 'INSERT' THEN
    INSERT INTO transaction_status_history (transaction_id, from_status, to_status, reason)
    VALUES (NEW.id, NULL, NEW.status, 'created');
  ELSIF TG_OP = 'UPDATE' AND OLD.status IS DISTINCT FROM NEW.status THEN
    INSERT INTO transaction_status_history (transaction_id, from_status, to_status, reason)
    VALUES (NEW.id, OLD.status, NEW.status, NEW.failure_reason);
  END IF;
  RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER transactions_status_history_insert
  AFTER INSERT ON transactions
  FOR EACH ROW
  EXECUTE FUNCTION record_transaction_status_change();

CREATE TRIGGER transactions_status_history_update
  AFTER UPDATE OF status ON transactions
  FOR EACH ROW
  EXECUTE FUNCTION record_transaction_status_change();

-- ---------------------------------------------------------------------------
-- 3. Ledger zero-sum check (deferred to end of statement / transaction)
-- ---------------------------------------------------------------------------
-- Each "group" (transaction_id or journal_id) must net to zero per currency.
-- DEFERRABLE INITIALLY DEFERRED means we can insert several rows in a
-- multi-row INSERT or across statements within a transaction, and the check
-- only runs at COMMIT.
CREATE OR REPLACE FUNCTION enforce_ledger_balance()
RETURNS TRIGGER AS $$
DECLARE
  imbalance RECORD;
BEGIN
  SELECT
    COALESCE(le.transaction_id, le.journal_id) AS group_id,
    le.currency,
    SUM(le.amount) AS net
  INTO imbalance
  FROM ledger_entries le
  WHERE COALESCE(le.transaction_id, le.journal_id) IN (
    SELECT DISTINCT COALESCE(transaction_id, journal_id) FROM new_rows
  )
  GROUP BY COALESCE(le.transaction_id, le.journal_id), le.currency
  HAVING SUM(le.amount) <> 0
  LIMIT 1;

  IF FOUND THEN
    RAISE EXCEPTION 'ledger imbalance for group % currency %: net=%',
                    imbalance.group_id, imbalance.currency, imbalance.net
      USING ERRCODE = 'check_violation';
  END IF;

  RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE CONSTRAINT TRIGGER ledger_zero_sum_check
  AFTER INSERT ON ledger_entries
  DEFERRABLE INITIALLY DEFERRED
  REFERENCING NEW TABLE AS new_rows
  FOR EACH STATEMENT
  EXECUTE FUNCTION enforce_ledger_balance();

-- ---------------------------------------------------------------------------
-- 4. Reject UPDATE/DELETE on ledger_entries (append-only invariant)
-- ---------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION reject_ledger_mutation()
RETURNS TRIGGER AS $$
BEGIN
  RAISE EXCEPTION 'ledger entries are append-only; cannot % entry %',
                  TG_OP, COALESCE(OLD.id::text, NEW.id::text)
    USING ERRCODE = 'check_violation';
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER ledger_no_update
  BEFORE UPDATE ON ledger_entries
  FOR EACH ROW EXECUTE FUNCTION reject_ledger_mutation();

CREATE TRIGGER ledger_no_delete
  BEFORE DELETE ON ledger_entries
  FOR EACH ROW EXECUTE FUNCTION reject_ledger_mutation();

-- ===========================================================================
-- VIEWS
-- ===========================================================================

-- account_balances — the ONLY source of truth for "how much money does this
-- account have." Derived; never cache without a reconciliation job.
CREATE VIEW account_balances AS
SELECT
  a.id                          AS account_id,
  a.currency,
  COALESCE(SUM(le.amount), 0)   AS balance
FROM accounts a
LEFT JOIN ledger_entries le ON le.account_id = a.id
GROUP BY a.id, a.currency;
