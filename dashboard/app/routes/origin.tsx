import { useState } from "react";
import { FileText } from "lucide-react";
import { Card, CardContent } from "~/components/ui/card";
import { api, fmtTime } from "~/lib/api";
import { cn } from "~/lib/utils";
import type { OriginFileMeta } from "~/lib/types";
import type { Route } from "./+types/origin";

export async function clientLoader({ params }: Route.ClientLoaderArgs) {
  const { files } = await api.originFiles(params.host);
  return { host: params.host, files };
}

export default function OriginRoute({ loaderData }: Route.ComponentProps) {
  const { host, files } = loaderData;
  const [selected, setSelected] = useState<string | null>(null);
  const [content, setContent] = useState<string>("");
  const [loading, setLoading] = useState(false);

  async function open(f: OriginFileMeta) {
    setSelected(f.path);
    setLoading(true);
    try {
      const res = await api.originFile(host, f.path);
      setContent(res.content);
    } catch {
      setContent("(could not load file)");
    } finally {
      setLoading(false);
    }
  }

  return (
    <div className="mx-auto max-w-4xl p-6">
      <header className="mb-5">
        <h1 className="text-xl font-semibold tracking-tight">
          Memory <span className="font-mono text-muted-foreground">· {host}</span>
        </h1>
        <p className="mt-1 text-sm text-muted-foreground">
          Files the agent saved for this site, reused across runs.
        </p>
      </header>

      {files.length === 0 ? (
        <p className="text-sm text-muted-foreground">No files stored for this site yet.</p>
      ) : (
        <div className="grid grid-cols-1 gap-4 md:grid-cols-[minmax(220px,300px)_1fr]">
          <div className="flex flex-col gap-1">
            {files.map((f) => (
              <button
                key={f.path}
                onClick={() => open(f)}
                className={cn(
                  "rounded-lg border p-2.5 text-left transition-colors",
                  selected === f.path
                    ? "border-primary/40 bg-accent"
                    : "border-transparent hover:border-border hover:bg-accent/50",
                )}
              >
                <div className="flex items-center gap-2 text-sm">
                  <FileText className="size-4 shrink-0 text-muted-foreground" />
                  <span className="truncate font-mono">{f.path}</span>
                </div>
                <div className="mt-1 pl-6 text-[11px] text-muted-foreground">
                  {f.size} B · {fmtTime(f.updated_at)}
                </div>
              </button>
            ))}
          </div>

          <Card className="min-h-40">
            <CardContent className="py-4">
              {selected == null ? (
                <p className="text-sm text-muted-foreground">Select a file to view its contents.</p>
              ) : loading ? (
                <p className="text-sm text-muted-foreground">Loading…</p>
              ) : (
                <pre className="overflow-auto text-xs leading-relaxed whitespace-pre-wrap break-words">
                  {content}
                </pre>
              )}
            </CardContent>
          </Card>
        </div>
      )}
    </div>
  );
}
