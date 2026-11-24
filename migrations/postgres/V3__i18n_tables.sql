-- V3 译名表（开发文档 §12.7）：频道与 EPG 节目的人工/机器译名，有译名优先，无则回退原始名。
CREATE TABLE IF NOT EXISTS channel_i18n (
    id BIGSERIAL PRIMARY KEY,
    channel_id BIGINT NOT NULL,
    locale VARCHAR(16) NOT NULL,
    name TEXT NOT NULL,
    UNIQUE (channel_id, locale)
);

CREATE TABLE IF NOT EXISTS epg_program_i18n (
    id BIGSERIAL PRIMARY KEY,
    program_id BIGINT NOT NULL,
    locale VARCHAR(16) NOT NULL,
    title TEXT NOT NULL,
    UNIQUE (program_id, locale)
);
