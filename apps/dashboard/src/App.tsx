import { Navigate, Route, Routes } from "react-router-dom";
import { useAuth } from "./auth";
import { Login } from "./pages/Login";
import { Layout } from "./components/Layout";
import { Overview } from "./pages/Overview";
import { Briefs } from "./pages/Briefs";
import { Mandates } from "./pages/Mandates";
import { Agents } from "./pages/Agents";
import { Lattice } from "./pages/Lattice";
import { Company } from "./pages/Company";
import { Costs } from "./pages/Costs";
import { Assign } from "./pages/Assign";
import { Runs } from "./pages/Runs";
import { Approvals } from "./pages/Approvals";
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
        <Route path="/mandates" element={<Mandates />} />
        <Route path="/briefs" element={<Briefs />} />
        <Route path="/agents" element={<Agents />} />
        <Route path="/lattice" element={<Lattice />} />
        <Route path="/company" element={<Company />} />
        <Route path="/costs" element={<Costs />} />
        <Route path="/assign" element={<Assign />} />
        <Route path="/runs" element={<Runs />} />
        <Route path="/approvals" element={<Approvals />} />
        <Route path="/chat" element={<Chat />} />
        <Route path="/scheduled" element={<Scheduled />} />
        <Route path="/settings" element={<Settings />} />
        <Route path="*" element={<Navigate to="/" replace />} />
      </Routes>
    </Layout>
  );
}
