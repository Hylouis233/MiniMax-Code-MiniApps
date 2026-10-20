// @ts-check
/**
 * 蹬王之王 local MCP 端点。
 *
 * # 为什么需要它（2026-10-20 实测踩到的坑）
 *
 * MiniApp 的 manifest 可以声明 `mcpEndpoints`，让**别的**插件在它们的回合里
 * 用到本插件的 MCP 工具。而 Host 在发布时会检查「声明的端点是否真的就绪」
 * （探活方式 = MCP 的 `initialize` + `tools/list`）。
 *
 * ⚠️ 而且 Host 的「已发布快照」是**粘的**：一旦某个版本的 manifest 声明过
 * 端点，后续版本**必须继续声明同样的端点**，否则发布会被
 * `MCP_PUBLICATION_INVALID` 挡下（`used.size !== expected.size`）。
 * ⇒ 声明了就得**真的实现**，否则端点探活失败，同样发布不了。
 *
 * # 这个端点提供什么
 *
 * | 工具 | 作用 |
 * | --- | --- |
 * | `watchdog_status` | 查当前额度、重置卡、以及「哪些 bot 挂掉了、待唤醒队列长什么样」 |
 * | `watchdog_wake` | **立即**跑一轮 Bot 唤醒（不等 60 秒的巡检） |
 * | `watchdog_decide` | 立即判定一次：现在该不该用卡、用哪张、为什么（**只判定不扣卡**） |
 *
 * 这样别的插件（例如 minimax-bot）可以在自己的回合里直接问蹬王之王
 * 「额度还剩多少」「哪些 bot 挂了」，而不必读文件。
 */

const PROTOCOL_VERSIONS = ['2025-06-18', '2025-03-26', '2024-11-05'];

/** 工具定义（模型可见，所以用中文）。 */
function toolDefs() {
  return [
    {
      name: 'watchdog_status',
      description:
        '查蹬王之王的当前状态：5 小时额度用量、重置卡与积分的到期时间、' +
        '以及待唤醒的 bot 列表（挂了但还没叫醒的）。',
      inputSchema: { type: 'object', properties: {}, required: [], additionalProperties: false },
    },
    {
      name: 'watchdog_wake',
      description:
        '立即跑一轮 Bot 唤醒：把「已挂掉但还没叫醒」的 bot 叫醒（复用它们自己的会话，不新建会话）。',
      inputSchema: { type: 'object', properties: {}, required: [], additionalProperties: false },
    },
    {
      name: 'watchdog_decide',
      description:
        '立即判定一次额度：按「先过期先用」算出现在该不该用重置卡、用哪一张、为什么。' +
        '**只判定不扣卡** —— 真要扣请用面板上的按钮。',
      inputSchema: { type: 'object', properties: {}, required: [], additionalProperties: false },
    },
  ];
}

/**
 * 创建 MCP 处理器。
 *
 * @param {{
 *   status: () => any,
 *   wake: () => Promise<any>,
 *   decide: () => Promise<any>,
 * }} deps
 */
export function createMcpHandler(deps) {
  /** @param {any} message */
  async function handleRpc(message) {
    if (!message || typeof message !== 'object' || message.jsonrpc !== '2.0') {
      return { jsonrpc: '2.0', id: null, error: { code: -32600, message: 'Invalid Request' } };
    }
    const { id, method, params } = message;
    const isNotification = id === undefined || id === null;
    try {
      let result;
      switch (method) {
        case 'initialize': {
          const requested = params?.protocolVersion;
          result = {
            protocolVersion: PROTOCOL_VERSIONS.includes(requested) ? requested : PROTOCOL_VERSIONS[0],
            capabilities: { tools: { listChanged: false } },
            serverInfo: { name: 'expiry-guardian', version: '1.6.0' },
            instructions:
              '蹬王之王：追踪额度与重置卡到期时间，并守护挂掉的 MiniMax Bot。' +
              '用 watchdog_status 查状态、watchdog_wake 立即叫醒挂掉的 bot、watchdog_use_card 立即用掉该用的重置卡。',
          };
          break;
        }
        case 'ping':
          result = {};
          break;
        case 'tools/list':
          result = { tools: toolDefs() };
          break;
        case 'tools/call': {
          const name = params?.name;
          const args = params?.arguments ?? {};
          if (name === 'watchdog_status') {
            // ⚠️ **必须 await**（2026-10-20 实测发现）：
            // `deps.status()` 是 async ⇒ 不 await 拿到的是 **Promise**，
            // 而 `JSON.stringify(Promise)` 得到的是字符串 `"{}"`
            // ⇒ 工具永远返回空对象，调用方什么也看不到。
            result = { content: [{ type: 'text', text: JSON.stringify(await deps.status(), null, 2) }] };
          } else if (name === 'watchdog_wake') {
            const r = await deps.wake();
            result = { content: [{ type: 'text', text: JSON.stringify(r, null, 2) }] };
          } else if (name === 'watchdog_decide') {
            const r = await deps.decide();
            result = { content: [{ type: 'text', text: JSON.stringify(r, null, 2) }] };
          } else {
            return { jsonrpc: '2.0', id, error: { code: -32602, message: `Unknown tool: ${name}` } };
          }
          break;
        }
        default:
          if (isNotification) return undefined;
          return { jsonrpc: '2.0', id, error: { code: -32601, message: `Method not found: ${method}` } };
      }
      return isNotification ? undefined : { jsonrpc: '2.0', id, result };
    } catch (error) {
      return {
        jsonrpc: '2.0',
        id,
        error: { code: -32603, message: error instanceof Error ? error.message : String(error) },
      };
    }
  }

  /**
   * 处理一次 MCP HTTP 请求。
   *
   * @param {any} request
   * @param {any} response
   * @param {any} body 已解析的请求体（POST 时）
   */
  return async function handleMcp(request, response, body) {
    const messages = Array.isArray(body) ? body : body ? [body] : [];
    if (messages.length === 0) {
      response.writeHead(400, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ error: 'empty request' }));
      return;
    }
    const out = [];
    for (const message of messages) {
      const r = await handleRpc(message);
      if (r !== undefined) out.push(r);
    }
    response.writeHead(200, {
      'content-type': 'application/json',
      'cache-control': 'no-store',
    });
    response.end(JSON.stringify(out.length === 1 ? out[0] : out));
  };
}
