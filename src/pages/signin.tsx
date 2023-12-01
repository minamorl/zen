import { useForm, SubmitHandler } from "react-hook-form";
import { trpc } from '../utils/trpc';
import { useEffect, useState } from 'react'
import { createClient } from '@supabase/supabase-js'
import { Auth } from '@supabase/auth-ui-react'
import { ThemeSupa } from "@supabase/auth-ui-shared";

type Inputs = {
  email: string
  password: string
};

const SUPABASE_ANON_KEY = 'eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJzdXBhYmFzZSIsInJlZiI6InNwaXd4bXRrbXltcWx0ZGZ4b2dnIiwicm9sZSI6ImFub24iLCJpYXQiOjE3MDEyNjk1NzMsImV4cCI6MjAxNjg0NTU3M30.aYkKWGpKvfoOj84p5T4lED0Iy6VXK8yH3Jo0h2kEQMc'
const supabase = createClient('https://spiwxmtkmymqltdfxogg.supabase.co', SUPABASE_ANON_KEY)

export default function SignInPage() {
  return (
  <Auth
    supabaseClient={supabase}
    appearance={{
      theme: ThemeSupa
    }}
    redirectTo="/"
  />
  );

  
}
