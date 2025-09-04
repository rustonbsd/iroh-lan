import { useState } from "react";
import { Button } from "@/components/ui/button";

function App() {
  const [count, setCount] = useState(0);
  return (
    <main className="h-screen flex items-center justify-center bg-background text-foreground">
      <div className="space-y-4 text-center">
        <h1 className="text-2xl font-semibold">Tauri + React + shadcn</h1>
        <p>Count: {count}</p>
        <div className="flex gap-3 justify-center">
          <Button variant="outline" onClick={() => setCount((c) => c + 1)}>Increment</Button>
          <Button variant="outline" onClick={() => setCount(0)}>
            Reset Count
          </Button>
        </div>
      </div>
    </main>
  );
}

export default App;