import { describe, expect, it } from "vitest";
import {
  baseToolName,
  buildToolDisplayModel,
  classifyToolName,
  stripToolResultHeader,
} from "@/components/message/toolDisplayModel";
import type { MessageItem } from "@/components/message/types";

describe("工具结果抬头与插件前缀", () => {
  it("去掉插件前缀后按原名分类，MCP 工具名保持不变", () => {
    expect(baseToolName("volcengine__generate_image")).toBe("generate_image");
    expect(baseToolName("terminal__run_shell")).toBe("run_shell");
    expect(classifyToolName("terminal__run_shell")).toBe("terminal");
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

  it("工具卡片展示去掉抬头后的输出", () => {
    const msg = {
      id: "t1",
      role: "tool", tool_name: "read_file",
      content: [{ type: "text", text: "调用工具 read_file：成功\nhello" }],
    } as unknown as MessageItem;
    const model = buildToolDisplayModel(msg, { path: "a.txt" });
    expect(model.outputText).toBe("hello");
    expect(model.variant).toBe("file-read");
  });
});
