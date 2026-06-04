import { Navigate, Route, Routes } from "react-router-dom";
import { useAuth } from "./auth";
import { Login } from "./pages/Login";
import { Layout } from "./components/Layout";
import { Overview } from "./pages/Overview";
import { Briefs } from "./pages/Briefs";
import { Agents } from "./pages/Agents";
import { Company } from "./pages/Company";
import { Assign } from "./pages/Assign";
import { Runs } from "./pages/Runs";
import { Chat } from "./pages/Chat";
import { Scheduled } from "./pages/Scheduled";
import { Settings } from "./pages/Settings";

export function App() {
  const { loading, status } = useAuth();

  if (loading) {
    return <div className="center-spinner">Loading Relix…</div>;
  }

  // Not logged in (or first-run setup needed) → the auth screen.
  if (!status?.authenticated) {
    return <Login />;
  }

  return (
    <Layout>
      <Routes>
        <Route path="/" element={<Overview />} />
        <Route path="/briefs" element={<Briefs />} />
        <Route path="/agents" element={<Agents />} />
        <Route path="/company" element={<Company />} />
        <Route path="/assign" element={<Assign />} />
        <Route path="/runs" element={<Runs />} />
        <Route path="/chat" element={<Chat />} />
        <Route path="/scheduled" element={<Scheduled />} />
        <Route path="/settings" element={<Settings />} />
        <Route path="*" element={<Navigate to="/" replace />} />
      </Routes>
    </Layout>
  );
}
