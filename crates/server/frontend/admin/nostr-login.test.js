"use strict";

const assert = require("node:assert/strict");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");
const { test } = require("node:test");
const vm = require("node:vm");

const source = readFileSync(join(__dirname, "nostr-login.js"), "utf8");
const origin = "https://admin.example.test";
const challengePath = "/api/admin/v1/auth/challenges";
const sessionPath = "/api/admin/v1/auth/sessions";
const challenge = {
  provider: "nostr",
  challenge_id: "11111111-1111-4111-8111-111111111111",
  challenge: "one-time-test-challenge",
  expires_at: "2026-09-05T12:01:00Z",
};
const session = {
  provider: "nostr",
  session_id: "22222222-2222-4222-8222-222222222222",
  user_id: "33333333-3333-4333-8333-333333333333",
};
const milliseconds = Date.parse("2026-09-05T12:00:00Z");
const plain = (value) => JSON.parse(JSON.stringify(value));
const signed = (draft) => ({ ...draft, id: "a".repeat(64), pubkey: "b".repeat(64), sig: "c".repeat(128) });
const json = (body, status = 201) => Response.json(body, { status });

function harness({ signer = signed, respond, paths = {}, controls = true } = {}) {
  const requests = [];
  const drafts = [];
  const navigations = [];
  let click;
  const button = {
    disabled: true,
    dataset: { challengePath, sessionPath, ...paths },
    addEventListener(type, callback) { assert.equal(type, "click"); click = callback; },
  };
  const feedback = { textContent: "" };
  vm.runInNewContext(source, {
    document: { getElementById: (id) => controls ? ({ "nostr-login": button, "nostr-login-status": feedback })[id] : null },
    window: {
      location: { origin, assign: (target) => navigations.push(target) },
      nostr: signer && { signEvent: async (draft) => { drafts.push(plain(draft)); return signer(draft); } },
    },
    fetch: async (url, options) => {
      requests.push({ url, options, payload: JSON.parse(options.body) });
      return respond ? respond(url, options, requests.length) : json(requests.length === 1 ? challenge : session);
    },
    Date: { now: () => milliseconds },
    URL, TextEncoder, TextDecoder, Uint8Array, AbortSignal,
  }, { timeout: 1000 });
  return { button, feedback, requests, drafts, navigations, click: () => click?.() };
}

test("browser sign-in binds the nonce, method, URL, and signed event without forwarding signer metadata", async () => {
  const app = harness({ signer: (draft) => ({ ...signed(draft), private_debug: "do-not-forward" }) });
  assert.equal(app.button.disabled, false);
  await app.click();
  assert.deepEqual(app.navigations, ["/admin"]);
  assert.equal(app.requests.length, 2);
  assert.equal(app.requests[0].url, origin + challengePath);
  assert.deepEqual(app.requests[0].payload, { provider: "nostr" });
  const draft = {
    created_at: milliseconds / 1000,
    kind: 27235,
    tags: [["u", origin + sessionPath], ["method", "POST"], ["challenge", challenge.challenge]],
    content: "",
  };
  assert.deepEqual(app.drafts, [draft]);
  assert.equal(app.requests[1].url, origin + sessionPath);
  assert.equal(typeof app.requests[1].payload.event, "string");
  assert.deepEqual({ ...app.requests[1].payload, event: JSON.parse(app.requests[1].payload.event) }, {
    provider: "nostr", challenge_id: challenge.challenge_id, challenge: challenge.challenge,
    event: signed(draft),
  });
  for (const { options } of app.requests) {
    assert.equal(options.method, "POST");
    assert.equal(options.redirect, "error");
    assert.equal(options.mode, "same-origin");
    assert.equal(options.credentials, "same-origin");
    assert.equal(options.cache, "no-store");
    assert.equal(options.headers["content-type"], "application/json");
    assert.ok(options.signal instanceof AbortSignal);
    assert.equal(options.signal.aborted, false);
    assert.equal(options.body.includes("do-not-forward"), false);
  }
});

test("missing and cancelled signers show safe recovery without submitting a session", async () => {
  for (const signer of [null, () => { throw new Error("private signer diagnostic"); }]) {
    const app = harness({ signer });
    await app.click();
    assert.match(app.feedback.textContent, /Unlock a Nostr signer/);
    assert.equal(app.feedback.textContent.includes("private signer diagnostic"), false);
    assert.equal(app.requests.length, signer ? 1 : 0);
    assert.deepEqual(app.navigations, []);
    assert.equal(app.button.disabled, false);
  }
});

test("duplicate clicks cannot start overlapping challenges", async () => {
  let acceptChallenge;
  const response = new Promise((resolve) => { acceptChallenge = resolve; });
  const app = harness({ respond: (_url, _options, index) => index === 1 ? response : json(session) });
  const first = app.click();
  assert.equal(app.button.disabled, true);
  await app.click();
  assert.equal(app.requests.length, 1);
  acceptChallenge(json(challenge));
  await first;
  assert.equal(app.requests.length, 2);
  assert.deepEqual(app.navigations, ["/admin"]);
});

test("unrecognized challenges and response encodings never reach the signer", async () => {
  const responses = [
    () => json(null),
    () => json({ ...challenge, provider: "password" }),
    () => json({ ...challenge, challenge_id: "unknown" }),
    () => json({ ...challenge, challenge: "" }),
    () => json({ ...challenge, challenge: "x".repeat(257) }),
    () => new Response("not-json", { status: 201, headers: { "content-type": "application/json" } }),
    () => new Response(JSON.stringify(challenge), { status: 201, headers: { "content-type": "text/html" } }),
    () => new Response(new Uint8Array([255]), { status: 201, headers: { "content-type": "application/json" } }),
  ];
  for (const response of responses) {
    const app = harness({ respond: response });
    await app.click();
    assert.equal(app.drafts.length, 0);
    assert.equal(app.requests.length, 1);
    assert.deepEqual(app.navigations, []);
    assert.match(app.feedback.textContent, /could not be confirmed/);
  }
});

test("JSON reads accept the byte limit and cancel the first byte above it", async () => {
  const body = JSON.stringify(challenge);
  const accepted = harness({ respond: (_url, _options, index) => index === 1
    ? new Response(body.padEnd(8192, " "), { status: 201, headers: { "content-type": "application/json" } })
    : json(session) });
  await accepted.click();
  assert.deepEqual(accepted.navigations, ["/admin"]);
  let cancelled = false;
  const stream = new ReadableStream({
    start(controller) { controller.enqueue(new TextEncoder().encode(body.padEnd(8193, " "))); },
    cancel() { cancelled = true; },
  });
  const rejected = harness({ respond: () => new Response(stream, { status: 201, headers: { "content-type": "application/json" } }) });
  await rejected.click();
  assert.equal(cancelled, true);
  assert.equal(rejected.drafts.length, 0);
  assert.deepEqual(rejected.navigations, []);
});

test("altered signer proofs cannot replace the intended nonce or request", async () => {
  for (const mutate of [
    (event) => { event.kind += 1; },
    (event) => { event.created_at -= 1; },
    (event) => { event.content = "unexpected"; },
    (event) => { event.tags[0][1] = "https://other.example.test/sign-in"; },
    (event) => { event.tags[2][1] = "old-challenge"; },
    (event) => { event.tags.push(["payload", "unrelated-body"]); },
    (event) => { event.pubkey = "wrong-key"; },
    (event) => { delete event.sig; },
  ]) {
    const app = harness({ signer: (draft) => { const event = signed(draft); mutate(event); return event; } });
    await app.click();
    assert.equal(app.requests.length, 1);
    assert.match(app.feedback.textContent, /Unlock a Nostr signer/);
    assert.deepEqual(app.navigations, []);
  }
});

test("API failures and uncertain session responses never claim a completed sign-in", async () => {
  for (const [status, expected] of [[401, /not accepted/], [403, /not accepted/], [429, /busy/], [500, /not be confirmed/], [200, /not be confirmed/]]) {
    const app = harness({ respond: (_url, _options, index) => index === 1 ? json(challenge) : json({ message: "private diagnostic" }, status) });
    await app.click();
    assert.match(app.feedback.textContent, expected);
    assert.equal(app.feedback.textContent.includes("private diagnostic"), false);
    assert.deepEqual(app.navigations, []);
    assert.equal(app.button.disabled, false);
  }
  for (const invalid of [null, { ...session, provider: "password" }, { ...session, user_id: [session.user_id] }]) {
    const app = harness({ respond: (_url, _options, index) => json(index === 1 ? challenge : invalid) });
    await app.click();
    assert.deepEqual(app.navigations, []);
    assert.match(app.feedback.textContent, /not be confirmed/);
  }
  const network = harness({ respond: () => { throw new Error("private network diagnostic"); } });
  await network.click();
  assert.match(network.feedback.textContent, /not be confirmed/);
  assert.equal(network.feedback.textContent.includes("private network diagnostic"), false);
});

test("scripts outside the login page and foreign endpoints cannot begin sign-in", async () => {
  const absent = harness({ controls: false });
  await absent.click();
  assert.equal(absent.requests.length, 0);
  for (const paths of [
    { challengePath: "https://other.example.test/challenge" },
    { sessionPath: "https://other.example.test/session" },
    { sessionPath: sessionPath + "?unexpected=query" },
    { sessionPath: sessionPath + "#fragment" },
  ]) {
    const app = harness({ paths });
    await app.click();
    assert.equal(app.requests.length, 0);
    assert.equal(app.drafts.length, 0);
  }
});
