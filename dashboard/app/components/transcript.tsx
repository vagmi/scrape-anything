import {
  Code2,
  Image as ImageIcon,
  Terminal,
  User,
  Wrench,
} from "lucide-react";
import type { Turn } from "~/lib/types";

const EXT_TOOLS = new Set(["execute_script", "screenshot"]);

export function Transcript({ turns }: { turns: Turn[] }) {
  const visible = turns.filter(
    (t) => !(t.role === "extension" && (t.kind === "signals" || t.kind === "result")),
  );

  if (visible.length === 0) {
    return <p className="text-sm text-muted-foreground">No turns recorded.</p>;
  }

  return (
    <div className="flex flex-col gap-3">
      {visible.map((t) => (
        <TurnView key={t.turn_index} turn={t} />
      ))}
    </div>
  );
}

function TurnView({ turn }: { turn: Turn }) {
  const { role, kind, payload } = turn;

  if (role === "user" && kind === "message") {
    return (
      <Bubble icon={<User className="size-3.5" />} label="you" tone="user">
        <p className="whitespace-pre-wrap break-words">{payload?.content}</p>
      </Bubble>
    );
  }

  if (role === "llm" && kind === "assistant") {
    const calls: any[] = payload?.tool_calls ?? [];
    if (calls.length > 0) {
      return (
        <Bubble icon={<Wrench className="size-3.5" />} label="agent · tools" tone="agent">
          <div className="flex flex-col gap-2">
            {calls.map((c, i) => (
              <ToolCallView key={c.call_id ?? i} call={c} />
            ))}
          </div>
        </Bubble>
      );
    }
    if (payload?.text) {
      return (
        <Bubble icon={<Terminal className="size-3.5" />} label="agent" tone="agent">
          <p className="whitespace-pre-wrap break-words">{payload.text}</p>
        </Bubble>
      );
    }
    return null;
  }

  if (role === "tool" && kind === "tool_response") {
    const isImg = payload?.image_base64 != null;
    const fnName = payload?.fn_name || "tool";
    return (
      <Bubble
        icon={isImg ? <ImageIcon className="size-3.5" /> : <Code2 className="size-3.5" />}
        label={`result · ${fnName}`}
        tone="result"
      >
        {isImg ? (
          <img
            src={`data:${payload.mime || "image/png"};base64,${payload.image_base64}`}
            alt="screenshot"
            className="max-h-80 rounded-md border"
          />
        ) : (
          <ResultContent content={payload?.content ?? ""} />
        )}
      </Bubble>
    );
  }

  return null;
}

function ToolCallView({ call }: { call: any }) {
  const fnName = call?.fn_name ?? "";
  const args = call?.fn_arguments ?? {};
  const code = typeof args?.code === "string" ? args.code : null;
  const argStr = code ?? (Object.keys(args).length ? JSON.stringify(args, null, 2) : "");

  return (
    <div>
      <div className="flex items-center gap-1.5 font-mono text-xs text-primary">
        <span className="text-muted-foreground">→</span> {fnName}
      </div>
      {argStr && (
        <details className="mt-1">
          <summary className="cursor-pointer text-xs text-muted-foreground select-none">
            {code ? "code" : "args"}
          </summary>
          <pre className="mt-1 max-h-56 overflow-auto rounded-md bg-muted p-2 text-xs leading-relaxed whitespace-pre-wrap break-words">
            {argStr}
          </pre>
        </details>
      )}
    </div>
  );
}

function ResultContent({ content }: { content: string }) {
  // Pretty-print JSON tool output when possible.
  let display = content;
  try {
    display = JSON.stringify(JSON.parse(content), null, 2);
  } catch {
    /* keep raw */
  }
  const truncated = display.length > 4000 ? display.slice(0, 4000) + "\n…" : display;
  return (
    <pre className="max-h-52 overflow-auto rounded-md bg-muted p-2 text-xs leading-relaxed whitespace-pre-wrap break-words">
      {truncated}
    </pre>
  );
}

const TONES: Record<string, string> = {
  user: "border-indigo-200 bg-indigo-50/50 dark:border-indigo-500/20 dark:bg-indigo-500/10",
  agent: "border-border bg-card",
  result: "border-border bg-muted/40",
};

function Bubble({
  icon,
  label,
  tone,
  children,
}: {
  icon: React.ReactNode;
  label: string;
  tone: keyof typeof TONES;
  children: React.ReactNode;
}) {
  return (
    <div className={`rounded-lg border p-3 ${TONES[tone]}`}>
      <div className="mb-1.5 flex items-center gap-1.5 text-xs text-muted-foreground">
        {icon}
        <span>{label}</span>
      </div>
      <div className="text-sm text-foreground">{children}</div>
    </div>
  );
}
