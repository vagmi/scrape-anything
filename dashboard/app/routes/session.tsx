import { Download, ExternalLink, FolderOpen } from "lucide-react";
import { Link } from "react-router";
import { StatusBadge } from "~/components/status-badge";
import { Transcript } from "~/components/transcript";
import { Button } from "~/components/ui/button";
import { Card, CardContent } from "~/components/ui/card";
import { api, fmtTime } from "~/lib/api";
import type { Artifact } from "~/lib/types";
import type { Route } from "./+types/session";

export async function clientLoader({ params }: Route.ClientLoaderArgs) {
  const session = await api.session(params.id);
  let artifact: Artifact | null = null;
  try {
    artifact = await api.artifact(params.id);
  } catch {
    artifact = null; // no artifact for this session yet
  }
  return { session, artifact };
}

export default function SessionRoute({ loaderData }: Route.ComponentProps) {
  const { session, artifact } = loaderData;

  return (
    <div className="mx-auto max-w-3xl p-6">
      <header className="mb-5">
        <h1 className="text-xl font-semibold tracking-tight">{session.intention}</h1>
        <div className="mt-1.5 flex flex-wrap items-center gap-2 text-xs text-muted-foreground">
          <a
            href={session.url}
            target="_blank"
            rel="noreferrer"
            className="inline-flex items-center gap-1 font-mono hover:text-foreground hover:underline"
          >
            {session.url}
            <ExternalLink className="size-3" />
          </a>
          <StatusBadge status={session.status} />
          <span>{fmtTime(session.created_at)}</span>
        </div>
      </header>

      {artifact && (
        <Card className="mb-5">
          <CardContent className="flex flex-wrap items-center gap-3 py-4">
            <span className="text-sm font-medium">
              {artifact.item_count} {artifact.kind} rows extracted
            </span>
            <div className="ml-auto flex gap-2">
              <Button asChild size="sm" variant="outline">
                <a href={api.artifactDownloadUrl(artifact.artifact_id, "json")}>
                  <Download className="size-4" /> JSON
                </a>
              </Button>
              <Button asChild size="sm" variant="outline">
                <a href={api.artifactDownloadUrl(artifact.artifact_id, "csv")}>
                  <Download className="size-4" /> CSV
                </a>
              </Button>
            </div>
          </CardContent>
        </Card>
      )}

      <Transcript turns={session.turns} />

      <div className="mt-6 border-t border-border pt-4">
        <Button asChild variant="outline" size="sm">
          <Link to={`/origins/${encodeURIComponent(session.host)}`}>
            <FolderOpen className="size-4" /> Memory files for {session.host}
          </Link>
        </Button>
      </div>
    </div>
  );
}
