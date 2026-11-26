-- V5: 默认语言改成简体中文（用户 2026-11-26 决定，覆盖文档 §12 的默认 zh-Hant-TW）。
-- 线上库的 default_locale 是 V2 种子写死的 zh-Hant-TW，改种子改不到它，这里直接更新。
-- UPDATE 本身幂等（反复跑结果相同）；setting_value 存的是 JSON 编码字符串，故带双引号。
UPDATE system_setting SET setting_value = '"zh-Hans-CN"' WHERE setting_key = 'default_locale';
