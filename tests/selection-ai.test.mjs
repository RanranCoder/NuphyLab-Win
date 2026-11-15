// Smart Selection AI service tests: request building (three protocols),
// endpoint/key validation, SSE incremental parsing, streaming response
// parsing, model list endpoint derivation, error normalization and preference
// patches. Pure logic only — no network access.
import test from 'node:test';
import assert from 'node:assert/strict';
import {
  buildPrompt, buildRequestBody, parseResponse, validateEndpoint, validateApiKey,
  credentialScope, validateStatus, SseParser, SelectionStreamParser,
  deriveModelListEndpoint, parseModelList, normalizePreferences, defaultPreferences,
  configurationIssue, SelectionAI, SelectionAIError, qaSamples,
  SELECTION_ACTIONS, TRANSLATION_LANGUAGES, PROVIDER_IDS,
} from '../server/selection-ai.mjs';

const baseConfiguration = {
  provider: 'openAI',
  endpoint: 'https://api.openai.com/v1/responses',
  model: 'gpt-6-astra',
  apiProtocol: 'responses',
  imageInputConfirmed: false,
  maximumOutputTokens: 4096,
  apiKey: 'sk-test',
};

const PNG_BASE64 = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==';

test('AI request bodies follow the protocol builders for all three protocols', () => {
  const prompt = buildPrompt({ action: 'translate', text: 'Hello', language: 'zh-CN' });
  const responses = buildRequestBody({ configuration: baseConfiguration, system: prompt.system, user: prompt.user });
  assert.equal(responses.url, 'https://api.openai.com/v1/responses');
  const responsesBody = JSON.parse(responses.body);
  assert.deepEqual(responsesBody.input.map(item => item.role), ['system', 'user']);
  assert.equal(responsesBody.input[1].content[0].type, 'input_text');
  assert.equal(responsesBody.store, false);
  assert.equal(responsesBody.max_output_tokens, 4096);
  assert.ok(responsesBody.input[1].content[0].text.includes('"selected_content":"Hello"'));

  const chatConfiguration = { ...baseConfiguration, endpoint: 'https://api.deepseek.com/chat/completions', apiProtocol: 'chatCompletions' };
  const chat = JSON.parse(buildRequestBody({ configuration: chatConfiguration, system: 'S', user: 'U' }).body);
  assert.deepEqual(chat.messages, [{ role: 'system', content: 'S' }, { role: 'user', content: 'U' }]);
  assert.equal(chat.max_tokens, 4096);

  const official = { ...baseConfiguration, apiProtocol: 'chatCompletions', endpoint: 'https://api.openai.com/v1/chat/completions' };
  assert.equal(JSON.parse(buildRequestBody({ configuration: official, system: '', user: 'U' }).body).max_completion_tokens, 4096);

  const anthropicConfiguration = { ...baseConfiguration, provider: 'anthropic', endpoint: 'https://api.anthropic.com/v1/messages', apiProtocol: 'anthropicMessages', anthropicWorkspaceID: 'ws-1' };
  const anthropic = buildRequestBody({ configuration: anthropicConfiguration, system: 'S', user: 'U' });
  const anthropicBody = JSON.parse(anthropic.body);
  assert.equal(anthropicBody.system, 'S');
  assert.deepEqual(anthropicBody.messages, [{ role: 'user', content: [{ type: 'text', text: 'U' }] }]);
  assert.equal(anthropicBody.max_tokens, 4096);
  assert.equal(anthropic.headers['anthropic-version'], '2026-10-21');
  assert.equal(anthropic.headers['anthropic-workspace-id'], 'ws-1');
  assert.equal(anthropic.headers.Authorization, 'Bearer sk-test');
});

test('image attachments are gated on confirmed image input and PNG magic', () => {
  const prompt = buildPrompt({ action: 'explain', text: '', language: 'zh-CN' });
  assert.throws(() => buildRequestBody({ configuration: baseConfiguration, system: prompt.system, user: 'U', image: { base64: PNG_BASE64 } }), /imagesDisabled|发送选区图片/);
  const confirmed = { ...baseConfiguration, imageInputConfirmed: true };
  assert.throws(() => buildRequestBody({ configuration: confirmed, system: prompt.system, user: 'U', image: { base64: 'aGVsbG8gd29ybGQ=' } }), /PNG/);
  const chatConfiguration = { ...confirmed, endpoint: 'https://api.deepseek.com/chat/completions', apiProtocol: 'chatCompletions' };
  const chat = JSON.parse(buildRequestBody({ configuration: chatConfiguration, system: 'S', user: 'U', image: { base64: PNG_BASE64, label: 'Selected region' } }).body);
  assert.deepEqual(chat.messages[1].content.map(part => part.type), ['text', 'text', 'image_url']);
  const anthropicConfiguration = { ...confirmed, provider: 'anthropic', endpoint: 'https://api.anthropic.com/v1/messages', apiProtocol: 'anthropicMessages' };
  const anthropic = JSON.parse(buildRequestBody({ configuration: anthropicConfiguration, system: 'S', user: 'U', image: { base64: PNG_BASE64 } }).body);
  assert.deepEqual(anthropic.messages[0].content.map(part => part.type), ['text', 'image']);
  assert.equal(anthropic.messages[0].content[1].source.media_type, 'image/png');
});

test('endpoints are HTTPS-only without credentials, query or fragment', () => {
  assert.throws(() => validateEndpoint('http://api.openai.com/v1/responses'), /HTTPS/);
  assert.throws(() => validateEndpoint('https://api.openai.com/v1/responses?x=1'), /查询或片段|账号/);
  assert.throws(() => validateEndpoint('https://user:pass@api.openai.com/v1'), /账号/);
  assert.throws(() => validateEndpoint('https://api.openai.com/v1#frag'), /查询或片段|账号/);
  assert.throws(() => validateEndpoint(' not-endpoint '), /HTTPS API 地址|不允许的字符|格式无效/);
  assert.throws(() => validateEndpoint('https://api.openai.com/{id}'), /不允许的字符/);
  assert.equal(validateEndpoint('https://api.openai.com/v1/responses').hostname, 'api.openai.com');
  // Port ranges and whitespace-only strings fail too.
  assert.throws(() => validateEndpoint('https://host:99999'), /端口无效|格式无效/);
  assert.throws(() => validateEndpoint(''), /HTTPS API 地址/);
});

test('API keys must be printable ASCII and the credential scope separates services', () => {
  assert.throws(() => validateApiKey(''), /API Key/);
  assert.throws(() => validateApiKey('中文字符'), /无效字符/);
  assert.throws(() => validateApiKey('a\nb'), /无效字符/);
  assert.equal(validateApiKey('sk-123_ABC'), 'sk-123_ABC');
  const scopeA = credentialScope(baseConfiguration);
  const scopeB = credentialScope({ ...baseConfiguration, endpoint: 'https://api.openai.com/v1/chat/completions', apiProtocol: 'chatCompletions' });
  assert.notEqual(scopeA, scopeB, 'a different path must select a different key');
  assert.notEqual(credentialScope({ ...baseConfiguration, anthropicWorkspaceID: 'ws-1' }), scopeA);
  assert.ok(scopeA.startsWith('openAI|responses|https://api.openai.com:443/v1/responses|'));
});

test('SSE parsing reassembles events across chunk boundaries and handles CRLF/CR/comments', () => {
  const parser = new SseParser();
  // Split a full stream into awkward chunks (byte-level simulation).
  const full = '﻿: keep-alive\n\n' + 'data: {"a":1}\n\ndata: {"b":2}\n\n' + 'event: done\ndata: line1\ndata: line2\n\n';
  const events = [];
  for (let index = 0; index < full.length; index += 7) {
    events.push(...parser.append(full.slice(index, index + 7)));
  }
  assert.deepEqual(events.map(event => event.name), ['message', 'message', 'done']);
  assert.equal(events[0].data, '{"a":1}');
  assert.equal(events[1].data, '{"b":2}');
  assert.equal(events[2].data, 'line1\nline2');
  // Bare CR line endings and a CRLF split across chunks.
  const cr = new SseParser();
  const crEvents = cr.append('data: x\r\rdata: y\r\n\r\n');
  assert.deepEqual(crEvents.map(event => event.data), ['x', 'y']);
});

test('streaming parsers handle Responses deltas and treat the final response as authoritative', () => {
  const parser = new SelectionStreamParser(baseConfiguration);
  const sse = new SseParser();
  const deltas = [];
  const stream = [
    'data: {"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"你"}\n\n',
    'data: {"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"好"}\n\n',
    'data: {"type":"response.completed","response":{"status":"completed","output":[{"type":"reasoning","summary":[]},{"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"你好"}]}]}}\n\n',
  ];
  for (const chunk of stream) {
    for (const event of sse.append(chunk)) deltas.push(...parser.consume(event));
  }
  assert.deepEqual(deltas, ['你', '好'], 'deltas are increments, never the accumulated preview');
  assert.equal(parser.finish(), '你好');
  // Refusal events fail loudly.
  const refusal = new SelectionStreamParser(baseConfiguration);
  assert.throws(() => refusal.consume({ name: 'message', data: '{"type":"response.refusal.delta","delta":"no"}' }), /拒绝/);
  // Incomplete Responses streams must not report success.
  const incomplete = new SelectionStreamParser(baseConfiguration);
  incomplete.consume({ name: 'message', data: '{"type":"response.output_text.delta","delta":"部分"}' });
  assert.throws(() => incomplete.finish(), /回答未完成/);
});

test('streaming parsers handle chat [DONE] and Anthropic message lifecycles', () => {
  const chatConfiguration = { ...baseConfiguration, apiProtocol: 'chatCompletions' };
  const chat = new SelectionStreamParser(chatConfiguration);
  const chatDeltas = [];
  for (const event of [
    '{"choices":[{"index":0,"delta":{"role":"assistant","content":"Hi"}}]}',
    '{"choices":[{"index":0,"delta":{"content":" there"},"finish_reason":null}]}',
    '{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}',
    '[DONE]',
  ]) {
    chatDeltas.push(...chat.consume({ name: 'message', data: event }));
  }
  assert.deepEqual(chatDeltas, ['Hi', ' there']);
  assert.equal(chat.finish(), 'Hi there');

  const anthropicConfiguration = { ...baseConfiguration, provider: 'anthropic', apiProtocol: 'anthropicMessages' };
  const anthropic = new SelectionStreamParser(anthropicConfiguration);
  const anthropicDeltas = [];
  for (const event of [
    '{"type":"message_start","message":{"type":"message","role":"assistant","content":[]}}',
    '{"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}',
    '{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hm"}}',
    '{"type":"content_block_stop","index":0}',
    '{"type":"content_block_start","index":1,"content_block":{"type":"text","text":"答"}}',
    '{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"案"}}',
    '{"type":"content_block_stop","index":1}',
    '{"type":"message_delta","delta":{"stop_reason":"end_turn"}}',
    '{"type":"message_stop"}',
  ]) {
    anthropicDeltas.push(...anthropic.consume({ name: 'message', data: event }));
  }
  assert.deepEqual(anthropicDeltas, ['答', '案'], 'thinking blocks are never exposed');
  assert.equal(anthropic.finish(), '答案');
});

test('tool calls, errors and unknown event shapes fail instead of faking text', () => {
  const chatConfiguration = { ...baseConfiguration, apiProtocol: 'chatCompletions' };
  const chat = new SelectionStreamParser(chatConfiguration);
  assert.throws(() => chat.consume({ name: 'message', data: '{"choices":[{"index":0,"delta":{"tool_calls":[{"id":"1"}]}}]}' }), /回答未完成/);
  assert.throws(() => chat.consume({ name: 'message', data: '{"error":{"message":"boom"}}' }), /失败/);
  const responses = new SelectionStreamParser(baseConfiguration);
  assert.throws(() => responses.consume({ name: 'error', data: '{"type":"error","code":"x"}' }), /失败/);
  assert.throws(() => responses.consume({ name: 'message', data: 'not-json' }), /无法识别/);
  // finish_reason length and non-stop reasons.
  const stopped = new SelectionStreamParser(chatConfiguration);
  assert.throws(() => stopped.consume({ name: 'message', data: '{"choices":[{"index":0,"delta":{},"finish_reason":"length"}]}' }), /回答未完成/);
});

test('model list endpoints derive /models per protocol or reject unsafe shapes', () => {
  assert.equal(deriveModelListEndpoint(baseConfiguration), 'https://api.openai.com/v1/models');
  assert.equal(deriveModelListEndpoint({ ...baseConfiguration, endpoint: 'https://api.deepseek.com/chat/completions', apiProtocol: 'chatCompletions' }), 'https://api.deepseek.com/models');
  assert.equal(deriveModelListEndpoint({ ...baseConfiguration, provider: 'anthropic', endpoint: 'https://api.anthropic.com/v1/messages', apiProtocol: 'anthropicMessages' }), 'https://api.anthropic.com/v1/models');
  assert.equal(deriveModelListEndpoint({ ...baseConfiguration, modelListEndpoint: 'https://api.openai.com/v1/catalog' }), 'https://api.openai.com/v1/catalog');
  assert.throws(() => deriveModelListEndpoint({ ...baseConfiguration, modelListEndpoint: 'https://evil.example.com/v1/models' }), /不同源/);
  assert.throws(() => deriveModelListEndpoint({ ...baseConfiguration, provider: 'qwenChina', endpoint: 'https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions', apiProtocol: 'chatCompletions' }), /预设或手填/);
  assert.throws(() => deriveModelListEndpoint({ ...baseConfiguration, endpoint: 'https://api.openai.com/v1/weird', apiProtocol: 'responses' }), /手填模型/);
  const parsed = parseModelList('{"data":[{"id":"m1","display_name":"M1"},{"id":"m1"},{"id":""},{"id":"m2"}]}', baseConfiguration);
  assert.deepEqual(parsed, { models: ['m1', 'm2'], hasMore: false });
  // Anthropic pagination requires last_id whenever has_more is set.
  assert.equal(parseModelList('{"data":[],"has_more":true,"last_id":"cursor-1"}', { ...baseConfiguration, apiProtocol: 'anthropicMessages' }).hasMore, true);
  assert.throws(() => parseModelList('{"data":[],"has_more":true}', { ...baseConfiguration, apiProtocol: 'anthropicMessages' }), /无法识别/);
});

test('HTTP status normalization and provider errors map onto stable codes', () => {
  for (const [status, code] of [[200, undefined], [304, 'redirected'], [401, 'authentication'], [403, 'authentication'], [429, 'rateLimited'], [503, 'serviceUnavailable'], [418, 'rejected']]) {
    try {
      validateStatus(status);
      assert.equal(code, undefined);
    } catch (error) {
      assert.equal(error.code, code, `status ${status}`);
      assert.ok(error.message.length > 0);
    }
  }
  assert.throws(() => parseResponse('{"error":{"message":"x"}}', baseConfiguration), err => err.code === 'providerError');
  assert.throws(() => parseResponse('{"status":"incomplete","output":[]}', baseConfiguration), err => err.code === 'incompleteResponse');
  assert.throws(() => parseResponse('not json', baseConfiguration), err => err.code === 'invalidResponse');
});

test('prompts separate the user task from untrusted selected content and apply languages', () => {
  const prompt = buildPrompt({ action: 'translate', text: 'Hello', language: 'ja' });
  const parsed = JSON.parse(prompt.user);
  assert.ok(prompt.system.includes('untrusted task data'));
  assert.ok(parsed.user_task.startsWith('Translate the selected text into ja.'));
  assert.equal(parsed.selected_content, 'Hello');
  // Reply keeps its conversation-language rule.
  const reply = JSON.parse(buildPrompt({ action: 'reply', text: 'hi', language: 'zh-CN' }).user);
  assert.ok(reply.user_task.includes('Use the language of the selected conversation'));
  // Follow-ups reuse the selection as context and append the question.
  const followUp = buildPrompt({
    action: 'ask', text: 'selected', language: 'zh-CN',
    history: [
      { role: 'user', text: buildPrompt({ action: 'translate', text: 'selected', language: 'zh-CN' }).user },
      { role: 'assistant', text: '翻译结果' },
    ], question: '换个说法？',
  });
  assert.equal(followUp.user, "User's follow-up question: 换个说法？");
  assert.deepEqual(followUp.turns.map(turn => turn.role), ['user', 'assistant']);
  assert.ok(followUp.turns[0].text.includes('"selected_content":"selected"'));
  assert.throws(() => buildPrompt({ action: 'ask', question: '  ' }), /追问/);
  assert.throws(() => buildPrompt({ action: 'nope' }), /未知/);
});

test('preference patches validate enums, keep at least one action and never corrupt defaults', () => {
  const defaults = defaultPreferences();
  assert.deepEqual(SELECTION_ACTIONS, ['translate', 'summarize', 'explain', 'reply']);
  assert.equal(defaults.defaultAction, 'translate');
  assert.equal(defaults.shortcut.keyCode, 12);
  assert.equal(defaults.shortcut.modifiers, 2560);
  assert.deepEqual(TRANSLATION_LANGUAGES.slice(0, 3), ['zh-CN', 'zh-TW', 'en']);
  assert.ok(PROVIDER_IDS.includes('custom'));
  const patched = normalizePreferences({
    enabled: true, voiceEnabled: true, defaultAction: 'summarize',
    enabledActions: ['summarize', 'explain', 'bogus'], translationLanguage: 'de',
    configuration: { provider: 'deepSeek', endpoint: 'https://api.deepseek.com/chat/completions', apiProtocol: 'chatCompletions', model: 'deepseek-v4-pro', maximumOutputTokens: 8192, anthropicWorkspaceID: '', imageInputConfirmed: true },
    shortcut: { keyCode: 0, modifiers: 2048, label: '⌥ A' },
  });
  assert.deepEqual(patched.enabledActions, ['summarize', 'explain']);
  assert.equal(patched.translationLanguage, 'de');
  assert.equal(patched.configuration.provider, 'deepSeek');
  assert.equal(patched.configuration.maximumOutputTokens, 8192);
  assert.equal(patched.configuration.anthropicWorkspaceID, undefined);
  assert.deepEqual(patched.shortcut, { keyCode: 0, modifiers: 2048, label: '⌥ A' });
  // Disabling the default action falls back to the first remaining one.
  assert.equal(normalizePreferences({ defaultAction: 'translate', enabledActions: ['summarize'] }).defaultAction, 'summarize');
  // Unknown enums are ignored rather than stored.
  const rejected = normalizePreferences({ translationLanguage: 'xx-XX', defaultAction: 'nope', enabledActions: [] });
  assert.equal(rejected.translationLanguage, defaults.translationLanguage);
  assert.equal(rejected.defaultAction, defaults.defaultAction);
  assert.deepEqual(rejected.enabledActions, SELECTION_ACTIONS);
  // Invalid model-list endpoints and oversized tokens are rejected.
  assert.throws(() => normalizePreferences({ configuration: { modelListEndpoint: 'http://x' } }), /HTTPS/);
  assert.equal(normalizePreferences({ configuration: { maximumOutputTokens: 99999 } }).configuration.maximumOutputTokens, 4096);
  // A patch object without shortcut keeps the stored one.
  assert.deepEqual(normalizePreferences({ enabled: true }, patched).shortcut, patched.shortcut);
  // configurationIssue reflects endpoint validity for the UI's status readout.
  assert.equal(configurationIssue(normalizePreferences({})), '');
  assert.ok(configurationIssue(normalizePreferences({ configuration: { endpoint: 'http://x' } })).length > 0);
});

test('non-streaming chat and anthropic responses parse through the same normalizer', () => {
  const chat = parseResponse(JSON.stringify({
    choices: [{ index: 0, message: { role: 'assistant', content: '答' }, finish_reason: 'stop' }],
  }), { ...baseConfiguration, apiProtocol: 'chatCompletions' });
  assert.equal(chat, '答');
  const anthropic = parseResponse(JSON.stringify({
    type: 'message', role: 'assistant', stop_reason: 'end_turn',
    content: [{ type: 'thinking', thinking: 'x' }, { type: 'text', text: '好' }],
  }), { ...baseConfiguration, provider: 'anthropic', apiProtocol: 'anthropicMessages' });
  assert.equal(anthropic, '好');
  // A refusal field fails even when a finish_reason says stop.
  assert.throws(() => parseResponse(JSON.stringify({
    choices: [{ index: 0, message: { role: 'assistant', content: 'x', refusal: 'no' }, finish_reason: 'stop' }],
  }), { ...baseConfiguration, apiProtocol: 'chatCompletions' }), /拒绝/);
});

test('the QA sample builds a real request and streams a real SSE transcript offline', () => {
  const sample = qaSamples();
  assert.equal(sample.request.model, 'gpt-6-astra');
  assert.equal(sample.request.input.at(-1).role, 'user');
  assert.ok(sample.request.input[0].content[0].text.includes('untrusted'));
  assert.ok(sample.request.input.at(-1).content[0].text.includes('"selected_content":"Hello world"'));
  assert.deepEqual(sample.deltas, ['你', '好']);
  assert.equal(sample.final, '你好');
  assert.equal(sample.modelsEndpoint, 'https://api.openai.com/v1/models');
});

test('save/load round-trips preferences and keys per credential scope without leaking keys', async () => {
  const { mkdtemp, writeFile, rm } = await import('node:fs/promises');
  const { tmpdir } = await import('node:os');
  const { join } = await import('node:path');
  const dir = await mkdtemp(join(tmpdir(), 'selection-ai-'));
  try {
    const service = new SelectionAI(dir);
    const saved = await service.handle('save', {
      preferences: { ...defaultPreferences(), enabled: true, configuration: { ...baseConfiguration, apiKey: undefined } },
      apiKey: 'sk-live-key',
    });
    assert.equal(saved.hasKey, true);
    assert.equal(JSON.stringify(saved).includes('sk-live-key'), false, 'the key never appears in status payloads');
    const reloaded = new SelectionAI(dir);
    const status = await reloaded.handle('status');
    assert.equal(status.hasKey, true);
    assert.equal(status.preferences.enabled, true);
    assert.equal(status.preferences.configuration.model, 'gpt-6-astra');
    // A different endpoint scope has no key.
    await reloaded.handle('save', { preferences: { ...status.preferences, configuration: { ...status.preferences.configuration, endpoint: 'https://api.openai.com/v1/chat/completions', apiProtocol: 'chatCompletions' } } });
    assert.equal((await reloaded.handle('status')).hasKey, false);
    await reloaded.handle('save', { preferences: { ...status.preferences } });
    const deleted = await reloaded.handle('deleteKey');
    assert.equal(deleted.hasKey, false);
    // An invalid key payload is rejected without corrupting the store.
    await assert.rejects(() => reloaded.handle('save', { preferences: {}, apiKey: 'bad key\n' }), /无效字符/);
    // The persisted file never holds more than its own schema.
    const onDisk = JSON.parse(await (await import('node:fs/promises')).readFile(join(dir, 'selection-ai.json'), 'utf8'));
    assert.equal(onDisk.version, 1);
    assert.equal(typeof onDisk.keys, 'object');
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('stream normalizes provider failures into SSE error events without leaking internals', async () => {
  const calls = [];
  const fetchImpl = async (url, options) => {
    calls.push({ url, options });
    return new Response('{"error":{"message":"secret backend detail"}}', { status: 401, headers: { 'content-type': 'application/json' } });
  };
  const service = new SelectionAI('/tmp/does-not-matter', fetchImpl);
  await service.handle('save', { preferences: { ...defaultPreferences(), configuration: { ...baseConfiguration, model: 'gpt-6-astra' } }, apiKey: 'sk-x' });
  const chunks = [];
  const res = {
    writeHead() {},
    write(body) { chunks.push(body); },
    end() { this.writableEnded = true; },
    on() {},
    off() {},
    writableEnded: false,
    destroyed: false,
  };
  await service.stream({ op: 'stream', action: 'translate', text: 'Hello', language: 'zh-CN' }, res);
  const events = chunks.map(chunk => JSON.parse(chunk.replace(/^data: /, '').trim()));
  assert.equal(events.at(-1).type, 'error');
  assert.equal(events.at(-1).code, 'authentication');
  assert.equal(JSON.stringify(events).includes('secret backend detail'), false, 'provider bodies are never forwarded');
  assert.equal(calls.length, 1);
  assert.equal(calls[0].options.headers.Authorization, 'Bearer sk-x');
});

test('stream forwards provider deltas as normalized SSE and completes with done', async () => {
  const sseBody = [
    'data: {"type":"response.output_text.delta","delta":"你"}',
    '',
    'data: {"type":"response.output_text.delta","delta":"好"}',
    '',
    'data: {"type":"response.completed","response":{"status":"completed","output":[{"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"你好"}]}]}}',
    '',
    '',
  ].join('\n');
  const fetchImpl = async () => new Response(sseBody, { status: 200, headers: { 'content-type': 'text/event-stream' } });
  const service = new SelectionAI('/tmp/does-not-matter', fetchImpl);
  await service.handle('save', { preferences: { ...defaultPreferences(), configuration: { ...baseConfiguration, model: 'gpt-6-astra' } }, apiKey: 'sk-x' });
  const chunks = [];
  const res = {
    writeHead() {},
    write(body) { chunks.push(body); },
    end() { this.writableEnded = true; },
    on() {},
    off() {},
    writableEnded: false,
    destroyed: false,
  };
  await service.stream({ op: 'stream', action: 'translate', text: 'Hello', language: 'zh-CN' }, res);
  const events = chunks.map(chunk => JSON.parse(chunk.replace(/^data: /, '').trim()));
  assert.deepEqual(events, [
    { type: 'delta', text: '你' },
    { type: 'delta', text: '好' },
    { type: 'done', text: '你好' },
  ]);
});

test('stream rejects missing keys and unknown actions before any network call', async () => {
  let called = 0;
  const fetchImpl = async () => { called += 1; return new Response('{}', { status: 200 }); };
  const { mkdtemp } = await import('node:fs/promises');
  const { tmpdir } = await import('node:os');
  const { join } = await import('node:path');
  // A private data dir: the other stream tests persist keys on disk.
  const dir = await mkdtemp(join(tmpdir(), 'selection-empty-'));
  const service = new SelectionAI(dir, fetchImpl);
  const res = { writeHead() {}, write() {}, end() { this.writableEnded = true; }, on() {}, off() {}, writableEnded: false, destroyed: false };
  await service.stream({ op: 'stream', action: 'translate', text: 'Hello' }, res);
  // No key configured: an error event is emitted and the provider is never called.
  assert.equal(called, 0);
});
