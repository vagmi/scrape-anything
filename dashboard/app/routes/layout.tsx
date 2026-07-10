import { useEffect, useState } from "react";
import { NavLink, Outlet, useNavigation, useSearchParams } from "react-router";
import { Search } from "lucide-react";
import { StatusBadge } from "~/components/status-badge";
import { Input } from "~/components/ui/input";
import { ScrollArea } from "~/components/ui/scroll-area";
import { api, fmtTime } from "~/lib/api";
import { cn } from "~/lib/utils";
import type { Route } from "./+types/layout";

export async function clientLoader({ request }: Route.ClientLoaderArgs) {
  const url = new URL(request.url);
  const q = url.searchParams.get("q") ?? "";
  const { sessions } = await api.sessions(q);
  return { sessions, q };
}

export default function Layout({ loaderData }: Route.ComponentProps) {
  const { sessions, q } = loaderData;
  const [params, setParams] = useSearchParams();
  const [term, setTerm] = useState(q);
  const nav = useNavigation();

  // Debounced live search: push ?q= into the URL, which re-runs clientLoader.
  useEffect(() => {
    const t = setTimeout(() => {
      const next = new URLSearchParams(params);
      if (term) next.set("q", term);
      else next.delete("q");
      if (next.toString() !== params.toString()) {
        setParams(next, { replace: true });
      }
    }, 250);
    return () => clearTimeout(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [term]);

  return (
    <div className="grid h-screen grid-cols-1 md:grid-cols-[340px_1fr] bg-background text-foreground">
      <aside className="flex min-h-0 flex-col border-r border-border">
        <div className="border-b border-border p-4">
          <div className="flex items-baseline gap-2">
            <NavLink to="/" className="text-lg font-semibold tracking-tight">
              Scrape Anything
            </NavLink>
            <span className="text-xs text-muted-foreground">history</span>
          </div>
          <div className="relative mt-3">
            <Search className="pointer-events-none absolute left-2.5 top-1/2 size-4 -translate-y-1/2 text-muted-foreground" />
            <Input
              value={term}
              onChange={(e) => setTerm(e.target.value)}
              placeholder="Search sessions…"
              className="pl-8"
            />
          </div>
        </div>

        <ScrollArea className="min-h-0 flex-1">
          <nav className="flex flex-col gap-1 p-2">
            {sessions.length === 0 ? (
              <p className="p-3 text-sm text-muted-foreground">No sessions found.</p>
            ) : (
              sessions.map((s) => (
                <NavLink
                  key={s.session_id}
                  to={`/sessions/${s.session_id}`}
                  className={({ isActive }) =>
                    cn(
                      "block rounded-lg border p-3 transition-colors",
                      isActive
                        ? "border-primary/40 bg-accent"
                        : "border-transparent hover:border-border hover:bg-accent/50",
                    )
                  }
                >
                  <div className="line-clamp-2 text-sm font-medium">{s.intention}</div>
                  <div className="mt-1.5 flex items-center gap-2 text-xs text-muted-foreground">
                    <span className="font-mono">{s.host}</span>
                    <StatusBadge status={s.status} />
                    <span className="ml-auto shrink-0">{s.item_count} items</span>
                  </div>
                  <div className="mt-1 text-[11px] text-muted-foreground">
                    {fmtTime(s.created_at)}
                  </div>
                </NavLink>
              ))
            )}
          </nav>
        </ScrollArea>
      </aside>

      <main
        className={cn(
          "min-h-0 overflow-y-auto transition-opacity",
          nav.state === "loading" && "opacity-60",
        )}
      >
        <Outlet />
      </main>
    </div>
  );
}
