-- Seed data for development and the dashboard demo.
-- ===================================================
-- Sets up:
--   1. Payway as an internal entity
--   2. Clearing accounts (NGN, USD, EUR, GBP) for in-flight value
--   3. A 'system_external_source' account representing money entering Payway
--      from outside (deposits/funding)
--   4. A sample Nigerian business with an opening NGN balance
--   5. USD/EUR/GBP clearing inventory so payments can complete
--   6. A sample foreign recipient
--
-- Opening balances are recorded as journal entries (transaction_id NULL),
-- which the ledger zero-sum check still validates per journal_id.

-- ---------------------------------------------------------------------------
-- 1. Supported currencies
-- ---------------------------------------------------------------------------
-- Adding a currency in production = a row here + a clearing account + a
-- system source account + an FX rate source. See learn/schema-design.md §3.
INSERT INTO currencies (code, name, exponent) VALUES
  ('NGN', 'Nigerian Naira',    2),
  ('USD', 'United States Dollar', 2),
  ('EUR', 'Euro',              2),
  ('GBP', 'Pound Sterling',    2);

-- ---------------------------------------------------------------------------
-- 2. Payway internal entity
-- ---------------------------------------------------------------------------
INSERT INTO business_entities (id, name, country_code, is_internal)
VALUES (
  '00000000-0000-0000-0000-000000000001',
  'Payway Internal',
  'NG',
  TRUE
);

-- ---------------------------------------------------------------------------
-- 3. Clearing accounts (one per supported currency)
-- ---------------------------------------------------------------------------
-- Adding a new currency = adding a clearing row here + an FX rate source.
INSERT INTO accounts (id, account_type, owner_business_id, currency, display_name)
VALUES
  ('00000000-0000-0000-0000-000000000100', 'clearing', '00000000-0000-0000-0000-000000000001', 'NGN', 'Payway NGN clearing'),
  ('00000000-0000-0000-0000-000000000101', 'clearing', '00000000-0000-0000-0000-000000000001', 'USD', 'Payway USD clearing'),
  ('00000000-0000-0000-0000-000000000102', 'clearing', '00000000-0000-0000-0000-000000000001', 'EUR', 'Payway EUR clearing'),
  ('00000000-0000-0000-0000-000000000103', 'clearing', '00000000-0000-0000-0000-000000000001', 'GBP', 'Payway GBP clearing');

-- ---------------------------------------------------------------------------
-- 4. System external source (counterparty for deposits/funding)
-- ---------------------------------------------------------------------------
-- Conceptually: every dollar we hold came from outside our system. This
-- account's running balance is the negative of total assets ever deposited.
-- It's the bookkeeping bridge that lets opening balances obey double-entry.
INSERT INTO accounts (id, account_type, currency, display_name)
VALUES
  ('00000000-0000-0000-0000-000000000200', 'system', 'NGN', 'External deposits source — NGN'),
  ('00000000-0000-0000-0000-000000000201', 'system', 'USD', 'External deposits source — USD'),
  ('00000000-0000-0000-0000-000000000202', 'system', 'EUR', 'External deposits source — EUR'),
  ('00000000-0000-0000-0000-000000000203', 'system', 'GBP', 'External deposits source — GBP');

-- ---------------------------------------------------------------------------
-- 5. Sample Nigerian business
-- ---------------------------------------------------------------------------
INSERT INTO business_entities (id, name, country_code, is_internal)
VALUES (
  '00000000-0000-0000-0000-000000000010',
  'Lagos Imports Ltd',
  'NG',
  FALSE
);

INSERT INTO accounts (id, account_type, owner_business_id, currency, display_name)
VALUES (
  '00000000-0000-0000-0000-000000000300',
  'user',
  '00000000-0000-0000-0000-000000000010',
  'NGN',
  'Lagos Imports — NGN operating'
);

-- ---------------------------------------------------------------------------
-- 6. Opening balances (journal entries)
-- ---------------------------------------------------------------------------
-- Lagos Imports deposits NGN 100,000,000 with Payway.
-- Journal entry: credit user account, debit external source.
INSERT INTO ledger_entries (journal_id, account_id, amount, currency, entry_type)
VALUES
  ('00000000-0000-0000-0000-000000000a01', '00000000-0000-0000-0000-000000000300',  100000000.0000, 'NGN', 'opening_balance_credit'),
  ('00000000-0000-0000-0000-000000000a01', '00000000-0000-0000-0000-000000000200', -100000000.0000, 'NGN', 'opening_balance_offset');

-- Pre-fund USD/EUR/GBP clearing so outbound payments can settle.
-- (In production, these are funded via FX trades or settlement banks.)
INSERT INTO ledger_entries (journal_id, account_id, amount, currency, entry_type)
VALUES
  ('00000000-0000-0000-0000-000000000a02', '00000000-0000-0000-0000-000000000101',  500000.0000, 'USD', 'inventory_credit'),
  ('00000000-0000-0000-0000-000000000a02', '00000000-0000-0000-0000-000000000201', -500000.0000, 'USD', 'inventory_offset'),
  ('00000000-0000-0000-0000-000000000a03', '00000000-0000-0000-0000-000000000102',  400000.0000, 'EUR', 'inventory_credit'),
  ('00000000-0000-0000-0000-000000000a03', '00000000-0000-0000-0000-000000000202', -400000.0000, 'EUR', 'inventory_offset'),
  ('00000000-0000-0000-0000-000000000a04', '00000000-0000-0000-0000-000000000103',  300000.0000, 'GBP', 'inventory_credit'),
  ('00000000-0000-0000-0000-000000000a04', '00000000-0000-0000-0000-000000000203', -300000.0000, 'GBP', 'inventory_offset');

-- ---------------------------------------------------------------------------
-- 7. Sample recipient (a US supplier)
-- ---------------------------------------------------------------------------
INSERT INTO recipients (id, name, country_code, bank_name, bank_account_number)
VALUES (
  '00000000-0000-0000-0000-000000000020',
  'Acme Components Inc',
  'US',
  'First National Bank',
  '****1234'                                            -- masked in seed; real values are encrypted
);

INSERT INTO accounts (id, account_type, owner_recipient_id, currency, display_name)
VALUES (
  '00000000-0000-0000-0000-000000000400',
  'external_recipient',
  '00000000-0000-0000-0000-000000000020',
  'USD',
  'Acme Components — USD receivable'
);
