export type Json =
  | string
  | number
  | boolean
  | null
  | { [key: string]: Json | undefined }
  | Json[];

export interface Database {
  public: {
    Tables: {
      boards: {
        Row: {
          created_at: string;
          description: string;
          id: string;
          name: string | null;
        };
        Insert: {
          created_at?: string;
          description?: string;
          id?: string;
          name?: string | null;
        };
        Update: {
          created_at?: string;
          description?: string;
          id?: string;
          name?: string | null;
        };
        Relationships: [];
      };
      personas: {
        Row: {
          created_at: string;
          id: string;
          name: string | null;
          user_id: string | null;
        };
        Insert: {
          created_at?: string;
          id?: string;
          name?: string | null;
          user_id?: string | null;
        };
        Update: {
          created_at?: string;
          id?: string;
          name?: string | null;
          user_id?: string | null;
        };
        Relationships: [
          {
            foreignKeyName: "personas_user_id_fkey";
            columns: ["user_id"];
            isOneToOne: false;
            referencedRelation: "users";
            referencedColumns: ["id"];
          },
        ];
      };
      posts: {
        Row: {
          board_id: string | null;
          created_at: string;
          id: string;
          persona_id: string | null;
          raw_text: string | null;
        };
        Insert: {
          board_id?: string | null;
          created_at?: string;
          id?: string;
          persona_id?: string | null;
          raw_text?: string | null;
        };
        Update: {
          board_id?: string | null;
          created_at?: string;
          id?: string;
          persona_id?: string | null;
          raw_text?: string | null;
        };
        Relationships: [
          {
            foreignKeyName: "posts_board_id_fkey";
            columns: ["board_id"];
            isOneToOne: false;
            referencedRelation: "boards";
            referencedColumns: ["id"];
          },
          {
            foreignKeyName: "posts_persona_id_fkey";
            columns: ["persona_id"];
            isOneToOne: false;
            referencedRelation: "personas";
            referencedColumns: ["id"];
          },
        ];
      };
      resources: {
        Row: {
          created_at: string;
          id: string;
          path: string | null;
          post_id: string | null;
        };
        Insert: {
          created_at?: string;
          id?: string;
          path?: string | null;
          post_id?: string | null;
        };
        Update: {
          created_at?: string;
          id?: string;
          path?: string | null;
          post_id?: string | null;
        };
        Relationships: [
          {
            foreignKeyName: "resources_post_id_fkey";
            columns: ["post_id"];
            isOneToOne: false;
            referencedRelation: "posts";
            referencedColumns: ["id"];
          },
        ];
      };
      threads: {
        Row: {
          created_at: string;
          id: string;
          persona_id: string | null;
          post_id: string | null;
          raw_text: string | null;
        };
        Insert: {
          created_at?: string;
          id?: string;
          persona_id?: string | null;
          post_id?: string | null;
          raw_text?: string | null;
        };
        Update: {
          created_at?: string;
          id?: string;
          persona_id?: string | null;
          post_id?: string | null;
          raw_text?: string | null;
        };
        Relationships: [
          {
            foreignKeyName: "threads_persona_id_fkey";
            columns: ["persona_id"];
            isOneToOne: false;
            referencedRelation: "personas";
            referencedColumns: ["id"];
          },
          {
            foreignKeyName: "threads_post_id_fkey";
            columns: ["post_id"];
            isOneToOne: false;
            referencedRelation: "posts";
            referencedColumns: ["id"];
          },
        ];
      };
    };
    Views: {
      [_ in never]: never;
    };
    Functions: {
      [_ in never]: never;
    };
    Enums: {
      [_ in never]: never;
    };
    CompositeTypes: {
      [_ in never]: never;
    };
  };
}

export type Tables<
  PublicTableNameOrOptions extends
    | keyof (Database["public"]["Tables"] & Database["public"]["Views"])
    | { schema: keyof Database },
  TableName extends PublicTableNameOrOptions extends { schema: keyof Database }
    ? keyof (Database[PublicTableNameOrOptions["schema"]]["Tables"] &
        Database[PublicTableNameOrOptions["schema"]]["Views"])
    : never = never,
> = PublicTableNameOrOptions extends { schema: keyof Database }
  ? (Database[PublicTableNameOrOptions["schema"]]["Tables"] &
      Database[PublicTableNameOrOptions["schema"]]["Views"])[TableName] extends {
      Row: infer R;
    }
    ? R
    : never
  : PublicTableNameOrOptions extends keyof (Database["public"]["Tables"] &
        Database["public"]["Views"])
    ? (Database["public"]["Tables"] &
        Database["public"]["Views"])[PublicTableNameOrOptions] extends {
        Row: infer R;
      }
      ? R
      : never
    : never;

export type TablesInsert<
  PublicTableNameOrOptions extends
    | keyof Database["public"]["Tables"]
    | { schema: keyof Database },
  TableName extends PublicTableNameOrOptions extends { schema: keyof Database }
    ? keyof Database[PublicTableNameOrOptions["schema"]]["Tables"]
    : never = never,
> = PublicTableNameOrOptions extends { schema: keyof Database }
  ? Database[PublicTableNameOrOptions["schema"]]["Tables"][TableName] extends {
      Insert: infer I;
    }
    ? I
    : never
  : PublicTableNameOrOptions extends keyof Database["public"]["Tables"]
    ? Database["public"]["Tables"][PublicTableNameOrOptions] extends {
        Insert: infer I;
      }
      ? I
      : never
    : never;

export type TablesUpdate<
  PublicTableNameOrOptions extends
    | keyof Database["public"]["Tables"]
    | { schema: keyof Database },
  TableName extends PublicTableNameOrOptions extends { schema: keyof Database }
    ? keyof Database[PublicTableNameOrOptions["schema"]]["Tables"]
    : never = never,
> = PublicTableNameOrOptions extends { schema: keyof Database }
  ? Database[PublicTableNameOrOptions["schema"]]["Tables"][TableName] extends {
      Update: infer U;
    }
    ? U
    : never
  : PublicTableNameOrOptions extends keyof Database["public"]["Tables"]
    ? Database["public"]["Tables"][PublicTableNameOrOptions] extends {
        Update: infer U;
      }
      ? U
      : never
    : never;

export type Enums<
  PublicEnumNameOrOptions extends
    | keyof Database["public"]["Enums"]
    | { schema: keyof Database },
  EnumName extends PublicEnumNameOrOptions extends { schema: keyof Database }
    ? keyof Database[PublicEnumNameOrOptions["schema"]]["Enums"]
    : never = never,
> = PublicEnumNameOrOptions extends { schema: keyof Database }
  ? Database[PublicEnumNameOrOptions["schema"]]["Enums"][EnumName]
  : PublicEnumNameOrOptions extends keyof Database["public"]["Enums"]
    ? Database["public"]["Enums"][PublicEnumNameOrOptions]
    : never;
