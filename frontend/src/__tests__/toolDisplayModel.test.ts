import { describe, expect, it } from "vitest";
import {
  argsSummary,
  baseToolName,
  buildRunningToolModel,
  buildToolDisplayModel,
  stripToolResultHeader,
} from "@/components/message/toolDisplayModel";
import type { MessageItem } from "@/components/message/types";

/** 运行时提供的参数摘要提示（测试固定一份）。 */
const hints = {
  lead_args: ["action"],
  skip_args: ["cwd", "limit"],
  hidden_value_prefixes: ["__tiangong_cwd="],
};

const toolMsg = (toolName: string, text: string): MessageItem =>
  ({
    id: `t-${toolName}`,
    role: "tool",
    tool_name: toolName,
    tool_call_id: "c1",
    content: [{ type: "text", text }],
  }) as unknown as MessageItem;

describe("工具结果抬头与插件前缀", () => {
  it("去掉插件前缀，MCP 工具名保持不变", () => {
    expect(baseToolName("volcengine__generate_image")).toBe("generate_image");
    expect(baseToolName("terminal__run_shell")).toBe("run_shell");
    expect(baseToolName("mcp__probe__read")).toBe("mcp__probe__read");
    expect(baseToolName("read_file")).toBe("read_file");
  });

  it("只去掉宿主添加的首行抬头", () => {
    expect(stripToolResultHeader("调用工具 read_file：成功\nfn main() {}")).toBe("fn main() {}");
    expect(stripToolResultHeader("调用插件 volcengine 的 generate_image：失败\n[tool_failure]")).toBe(
      "[tool_failure]",
    );
    expect(stripToolResultHeader("普通输出\n调用工具 x：成功")).toBe("普通输出\n调用工具 x：成功");
  });

  it("结果显示去掉抬头后的输出", () => {
    const model = buildToolDisplayModel(toolMsg("fs__file_read", "调用插件 fs 的 file_read：成功\nhello"), {
      action: "read",
      path: "a.txt",
    });
    expect(model.outputText).toBe("hello");
  });
});

describe("通用工具行", () => {
  it("按运行时提示组织摘要：lead 参数在前且只显示取值，跳过配置参数", () => {
    const model = buildRunningToolModel(
      "fs__file_write",
      { path: "docs/a.md", action: "replace", cwd: "/ws" },
      hints,
    );
    expect(model.title).toBe("工具调用");
    expect(model.toolName).toBe("fs__file_write");
    expect(model.summary).toBe("replace path=docs/a.md");
    expect(model.argsText).toContain('"path": "docs/a.md"');
    expect(model.outputText).toBeNull();
    expect(model.state).toBe("running");
    // 不按工具名推断展示（旧工具名同样通用展示）。
    expect(buildRunningToolModel("read_file", { path: "a.rs" }, hints).title).toBe("工具调用");
  });

  it("没有提示时按全部参数通用拼接", () => {
    const model = buildRunningToolModel("fs__file_write", { action: "replace", cwd: "/ws" });
    expect(model.summary).toBe("action=replace cwd=/ws");
  });

  it("完成后显示结果，失败时摘要为错误首行", () => {
    const ok = buildToolDisplayModel(toolMsg("terminal__run_shell", "Finished"), { script: "cargo check" });
    expect(ok.summary).toBe("script=cargo check");
    expect(ok.outputText).toBe("Finished");
    expect(ok.state).toBe("ok");
    const failed = buildToolDisplayModel(
      { ...toolMsg("x__t", "boom\nstack"), is_error: true } as MessageItem,
      {},
    );
    expect(failed.state).toBe("error");
    expect(failed.errorSummary).toBe("boom");
  });

  it("参数摘要：位置参数与对象参数", () => {
    expect(argsSummary(["ls", "-la", "__tiangong_cwd=/ws"], hints)).toBe("ls -la");
    expect(argsSummary({ query: "foo", limit: 5 }, hints)).toBe("query=foo");
    expect(argsSummary({})).toBeNull();
    expect(argsSummary(undefined)).toBeNull();
  });
});
