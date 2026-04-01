import { NavLink } from "react-router-dom";
import { type ReactNode } from "react";

interface LayoutProps {
  children: ReactNode;
  onLogout: () => void;
}

export default function Layout({ children, onLogout }: LayoutProps) {
  return (
    <div className="app-layout">
      <aside className="sidebar">
        <div className="sidebar-header">
          <h1>appbase</h1>
          <div className="subtitle">control plane</div>
        </div>
        <ul className="sidebar-nav">
          <li>
            <NavLink to="/" end className={({ isActive }) => isActive ? "active" : ""}>
              // overview
            </NavLink>
          </li>
          <li>
            <NavLink to="/apps" className={({ isActive }) => isActive ? "active" : ""}>
              // apps
            </NavLink>
          </li>
          <li>
            <NavLink to="/apps/new" className={({ isActive }) => isActive ? "active" : ""}>
              // create app
            </NavLink>
          </li>
        </ul>
        <div className="sidebar-footer">
          <button onClick={onLogout}>[logout]</button>
        </div>
      </aside>
      <main className="main-content">
        {children}
      </main>
    </div>
  );
}
