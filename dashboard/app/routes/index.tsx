import { Inbox } from "lucide-react";

export default function Index() {
  return (
    <div className="grid h-full place-items-center p-8">
      <div className="flex flex-col items-center gap-3 text-center text-muted-foreground">
        <Inbox className="size-10 opacity-40" />
        <p className="text-sm">Select a session to view its transcript and artifacts.</p>
      </div>
    </div>
  );
}
