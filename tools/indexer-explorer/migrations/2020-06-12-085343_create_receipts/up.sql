-- Your SQL goes here
CREATE TABLE receipts (
    receipt_id varchar(58) PRIMARY KEY NOT NULL,
    predecessor_id text,
    receiver_id text,
    status varchar(128),
    type varchar(6)
);

CREATE TABLE receipt_data (
    receipt_id varchar(58) PRIMARY KEY NOT NULL,
    data_id varchar(58) NOT NULL,
    data text
);

CREATE TABLE receipt_action (
    receipt_id varchar(58) PRIMARY KEY NOT NULL,
    signer_id varchar(58) NOT NULL,
    signer_public_key text NOT NULL,
    gas_price numeric(45, 0)
);

CREATE TABLE actions (
    receipt_id varchar(58) PRIMARY KEY NOT NULL,
    index int NOT NULL,
    type varchar(15) NOT NULL,
    args json
);


CREATE TABLE accounts (
    account_id text PRIMARY KEY NOT NULL,
    index int NOT NULL,
    created_by_receipt_id varchar(58) NOT NULL,
    created_at_timestamp numeric(45, 0)
);

CREATE TABLE access_keys (
    account_id text PRIMARY KEY NOT NULL,
    public_key text NOT NULL,
    access_key_type text NOT NULL
);
