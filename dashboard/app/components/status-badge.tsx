import { Badge } from "~/components/ui/badge";
import { cn } from "~/lib/utils";

const STYLES: Record<string, string> = {
  completed:
    "border-transparent bg-emerald-100 text-emerald-800 dark:bg-emerald-500/15 dark:text-emerald-300",
  active:
    "border-transparent bg-amber-100 text-amber-800 dark:bg-amber-500/15 dark:text-amber-300",
  gave_up:
    "border-transparent bg-rose-100 text-rose-800 dark:bg-rose-500/15 dark:text-rose-300",
};

export function StatusBadge({ status }: { status: string }) {
  return (
    <Badge
      variant="outline"
      className={cn("font-medium capitalize", STYLES[status])}
    >
      {status.replace("_", " ")}
    </Badge>
  );
}
