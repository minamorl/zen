export type Json =
  | string
  | number
  | boolean
  | null
  | { [key: string]: Json | undefined }
  | Json[]

export interface Database {
  public: {
    Tables: {
      personas: {
        Row: {
          created_at: string
          id: string
          name: string | null
          user_id: string | null
        }
        Insert: {
          created_at?: string
          id?: string
          name?: string | null
          user_id?: string | null
        }
        Update: {
          created_at?: string
          id?: string
          name?: string | null
          user_id?: string | null
        }
        Relationships: [
          {
            foreignKeyName: "personas_user_id_fkey"
            columns: ["user_id"]
            isOneToOne: false
            referencedRelation: "users"
            referencedColumns: ["id"]
          }
        ]
      }
      posts: {
        Row: {
          created_at: string
          id: string
          persona_id: string | null
          raw_text: string | null
        }
        Insert: {
          created_at?: string
          id?: string
          persona_id?: string | null
          raw_text?: string | null
        }
        Update: {
          created_at?: string
          id?: string
          persona_id?: string | null
          raw_text?: string | null
        }
        Relationships: [
          {
            foreignKeyName: "posts_persona_id_fkey"
            columns: ["persona_id"]
            isOneToOne: false
            referencedRelation: "personas"
            referencedColumns: ["id"]
          }
        ]
      }
      threads: {
        Row: {
          created_at: string
          id: string
          persona_id: string | null
          post_id: string | null
          raw_text: string | null
        }
        Insert: {
          created_at?: string
          id?: string
          persona_id?: string | null
          post_id?: string | null
          raw_text?: string | null
        }
        Update: {
          created_at?: string
          id?: string
          persona_id?: string | null
          post_id?: string | null
          raw_text?: string | null
        }
        Relationships: [
          {
            foreignKeyName: "threads_persona_id_fkey"
            columns: ["persona_id"]
            isOneToOne: false
            referencedRelation: "personas"
            referencedColumns: ["id"]
          },
          {
            foreignKeyName: "threads_post_id_fkey"
            columns: ["post_id"]
            isOneToOne: false
            referencedRelation: "posts"
            referencedColumns: ["id"]
          }
        ]
      }
    }
    Views: {
      [_ in never]: never
    }
    Functions: {
      [_ in never]: never
    }
    Enums: {
      [_ in never]: never
    }
    CompositeTypes: {
      [_ in never]: never
    }
  }
}
