// Heavy npm deps: lodash-es (full ESM lodash), date-fns, uuid
// Tests tree-shaking effectiveness on large libraries

import { groupBy, sortBy, uniqBy, keyBy } from "lodash-es";
import { format, formatDistanceToNow, parseISO } from "date-fns";
import { v4 as uuidv4 } from "uuid";

interface Task {
    id: string;
    title: string;
    category: string;
    createdAt: string;
    done: boolean;
}

const tasks: Task[] = [];

export function addTask(title: string, category: string): Task {
    const task: Task = {
        id: uuidv4(),
        title,
        category,
        createdAt: new Date().toISOString(),
        done: false,
    };
    tasks.push(task);
    return task;
}

export function listTasks(): Record<string, Task[]> {
    return groupBy(sortBy(tasks, "createdAt"), "category");
}

export function getTask(id: string): Task | undefined {
    const byId = keyBy(tasks, "id");
    return byId[id];
}

export function uniqueCategories(): string[] {
    return uniqBy(tasks, "category").map(t => t.category);
}

export function formatTask(task: Task): string {
    const date = parseISO(task.createdAt);
    const formatted = format(date, "yyyy-MM-dd HH:mm");
    const relative = formatDistanceToNow(date, { addSuffix: true });
    return `[${task.done ? "x" : " "}] ${task.title} (${formatted}, ${relative})`;
}

export function ping(): string {
    return "pong";
}
